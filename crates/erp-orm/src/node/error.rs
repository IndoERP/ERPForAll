//! The errors of the node registry.

use std::fmt;
use std::time::Duration;

use erp_fields::ErrorKind;

use super::{HeartbeatError, NodeId};

/// What went wrong in the node registry. Converts into [`erp_fields::Error`] with a
/// kind per variant and itself as the source.
///
/// ```
/// use erp_fields::ErrorKind;
/// use erp_orm::node::NodeError;
/// let e = NodeError::invalid_setting("node.lease_intervals", "node.lease_intervals = 2 is outside its bounds 3 – 10");
/// assert_eq!(e.to_string(), "node.lease_intervals = 2 is outside its bounds 3 – 10");
/// let converted: erp_fields::Error = e.into();
/// assert_eq!(converted.kind(), ErrorKind::Programming);
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum NodeError {
    /// A node table does not exist in the database: the schema sync did not run there.
    TableMissing {
        /// The database.
        database: String,
        /// The missing table.
        table: &'static str,
    },
    /// This identity already has a membership in the database: this process still holds one
    /// there (its owner handle was not dropped), or its row there is fresh and not `stopped` (an
    /// expired or `stopped` row of the identity is replaced by a new registration once no
    /// membership of this process holds it).
    AlreadyRegistered {
        /// The database.
        database: String,
        /// The node.
        node: NodeId,
    },
    /// Another process renews this node's row: the process was cloned.
    IdentityConflict {
        /// The database.
        database: String,
        /// The node.
        node: NodeId,
    },
    /// A dev identity tried to register where non-dev nodes are live (design §17 decision 5).
    DevOnLiveCluster {
        /// The database.
        database: String,
        /// The live non-dev nodes, `name (host h, pid p, role)`.
        nodes: Vec<String>,
    },
    /// A setting, a name or a bound is invalid.
    InvalidSetting {
        /// The setting.
        name: String,
        /// The whole message.
        message: String,
    },
    /// A setting or a settings section was described twice.
    DuplicateSetting {
        /// The section.
        section: String,
        /// The setting; `None` when the section itself was described twice.
        name: Option<String>,
    },
    /// A shared secret shorter than 32 bytes.
    ShortSharedSecret {
        /// The section.
        section: String,
        /// The setting.
        name: String,
        /// Its length in bytes.
        len: usize,
    },
    /// An addon source cannot be digested.
    AddonSource {
        /// The addon (or the path, for a dev folder entry).
        addon: String,
        /// What is wrong.
        message: String,
    },
    /// The build identity cannot be resolved.
    BuildIdentity {
        /// What is wrong.
        message: String,
        /// The I/O error, when reading the executable failed.
        source: Option<std::io::Error>,
    },
    /// A rollout window was refused.
    RolloutRefused {
        /// The database (empty when the window was refused before connecting).
        database: String,
        /// Why.
        reason: String,
    },
    /// A durable feature rollout operation was refused without changing its state.
    FeatureRefused {
        /// The database (empty when refused before connecting).
        database: String,
        /// The validated feature key.
        feature: String,
        /// The bounded policy diagnostic.
        reason: String,
    },
    /// A registry operation did not finish in time.
    Timeout {
        /// The database.
        database: String,
        /// The operation.
        what: &'static str,
        /// The bound that passed.
        after: Duration,
    },
    /// A renewal failed (the heartbeat goes on; readiness follows the failure's class).
    Heartbeat {
        /// The database.
        database: String,
        /// The failure.
        error: HeartbeatError,
    },
    /// The membership's heartbeat task has ended (after a shutdown, or it failed).
    MembershipEnded {
        /// The database.
        database: String,
    },
    /// A statement failed or returned something undecodable.
    Database {
        /// The database (empty when it is not known yet).
        database: String,
        /// The operation.
        what: &'static str,
        /// The database's message.
        message: String,
        /// The SQLSTATE, when the server sent one.
        sqlstate: Option<String>,
        /// The driver or pool error.
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

impl NodeError {
    /// An [`InvalidSetting`](Self::InvalidSetting) error.
    pub fn invalid_setting(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidSetting {
            name: name.into(),
            message: message.into(),
        }
    }

    /// The kind this error converts to.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::RolloutRefused { .. } | Self::DevOnLiveCluster { .. } => ErrorKind::UserError,
            // an operational condition (a cloned process), not a defect of the caller
            Self::IdentityConflict { .. } => ErrorKind::Conflict,
            Self::Timeout { .. } | Self::Heartbeat { .. } | Self::Database { .. } => {
                ErrorKind::Database
            }
            Self::TableMissing { .. }
            | Self::AlreadyRegistered { .. }
            | Self::InvalidSetting { .. }
            | Self::DuplicateSetting { .. }
            | Self::ShortSharedSecret { .. }
            | Self::AddonSource { .. }
            | Self::BuildIdentity { .. }
            | Self::MembershipEnded { .. }
            | Self::FeatureRefused { .. } => ErrorKind::Programming,
        }
    }

    /// The SQLSTATE of a database failure.
    pub fn sqlstate(&self) -> Option<&str> {
        match self {
            Self::Database { sqlstate, .. } => sqlstate.as_deref(),
            Self::Heartbeat { error, .. } => error.sqlstate(),
            _ => None,
        }
    }

    /// A failure of `what` in `database` from a driver error.
    pub(crate) fn pg(database: &str, what: &'static str, e: tokio_postgres::Error) -> Self {
        let (message, sqlstate) = match e.as_db_error() {
            Some(db) => (db.message().to_owned(), Some(db.code().code().to_owned())),
            None => (e.to_string(), None),
        };
        Self::Database {
            database: database.to_owned(),
            what,
            message,
            sqlstate,
            source: Some(Box::new(e)),
        }
    }

    /// A pool checkout failure of `what` in `database`.
    pub(crate) fn pool(
        database: &str,
        what: &'static str,
        e: deadpool_postgres::PoolError,
    ) -> Self {
        let sqlstate = match &e {
            deadpool_postgres::PoolError::Backend(b) => {
                b.as_db_error().map(|db| db.code().code().to_owned())
            }
            _ => None,
        };
        Self::Database {
            database: database.to_owned(),
            what,
            message: format!("could not get a database connection: {e}"),
            sqlstate,
            source: Some(Box::new(e)),
        }
    }

    /// A value read back that cannot be decoded (a broken invariant, never input).
    pub(crate) fn undecodable(
        database: &str,
        what: &'static str,
        detail: impl fmt::Display,
    ) -> Self {
        Self::Database {
            database: database.to_owned(),
            what,
            message: format!("undecodable result: {detail}"),
            sqlstate: None,
            source: None,
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TableMissing { database, table } => write!(
                f,
                "{table} is missing in database {database}: run the schema sync, \
                 `erp module upgrade base -d {database}`"
            ),
            Self::AlreadyRegistered { database, node } => write!(
                f,
                "node {node} is already registered in database {database}: one membership per \
                 process and database"
            ),
            Self::IdentityConflict { database, node } => write!(
                f,
                "database {database}: another process renews node {node} (its row is ahead of \
                 this process's beat): this process was probably cloned; restart it so it gets \
                 a new node id"
            ),
            Self::DevOnLiveCluster { database, nodes } => write!(
                f,
                "database {database}: erp dev refuses to join a database served by live non-dev \
                 nodes: {}; use a development database",
                nodes.join(", ")
            ),
            Self::InvalidSetting { message, .. } => f.write_str(message),
            Self::DuplicateSetting {
                section,
                name: Some(name),
            } => write!(f, "setting {name} is described twice in section {section}"),
            Self::DuplicateSetting {
                section,
                name: None,
            } => write!(f, "settings section {section} is described twice"),
            Self::ShortSharedSecret { name, len, .. } => write!(
                f,
                "shared secret {name} is {}: a shared secret must be a generated key \
                 of at least 32 bytes (passwords are node settings)",
                plural(*len, "byte", "bytes")
            ),
            Self::AddonSource { addon, message } => write!(f, "addon {addon}: {message}"),
            Self::BuildIdentity { message, .. } => write!(f, "build identity: {message}"),
            Self::RolloutRefused { database, reason } => {
                // refused before connecting: no database known yet
                if !database.is_empty() {
                    write!(f, "database {database}: ")?;
                }
                write!(f, "rollout window refused: {reason}")
            }
            Self::FeatureRefused {
                database,
                feature,
                reason,
            } => {
                if !database.is_empty() {
                    write!(f, "database {database}: ")?;
                }
                write!(f, "feature {feature} refused: {reason}")
            }
            Self::Timeout {
                database,
                what,
                after,
            } => {
                if !database.is_empty() {
                    write!(f, "database {database}: ")?;
                }
                write!(f, "{what} did not finish within {} ms", after.as_millis())
            }
            Self::Heartbeat { database, error } => write!(f, "database {database}: {error}"),
            Self::MembershipEnded { database } => write!(
                f,
                "database {database}: the node membership has ended (shut down, or its \
                 heartbeat task stopped)"
            ),
            Self::Database {
                database,
                what,
                message,
                ..
            } => {
                if database.is_empty() {
                    write!(f, "{what} failed: {message}")
                } else {
                    write!(f, "database {database}: {what} failed: {message}")
                }
            }
        }
    }
}

impl std::error::Error for NodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database {
                source: Some(source),
                ..
            } => Some(source.as_ref()),
            Self::BuildIdentity {
                source: Some(source),
                ..
            } => Some(source),
            _ => None,
        }
    }
}

impl From<NodeError> for erp_fields::Error {
    fn from(e: NodeError) -> Self {
        let sqlstate = e.sqlstate().map(str::to_owned);
        erp_fields::Error::new(e.kind(), e.to_string())
            .with_sqlstate(sqlstate)
            .with_source(e)
    }
}
