//! The rollout window and the node listing: operator commands (design §3, §7.5).

use std::time::Duration;

use deadpool_postgres::Pool;
use tokio_postgres::Row;
use tokio_postgres::types::{ToSql, Type};

use super::heartbeat::{
    Conn, checked_database, digest_col, get, ms, rollback, rows_at, split, timestamp, with_grace,
};
use super::sql::{self, Flight};
use super::{Digest, NodeError, NodeId, NodeRole, NodeState, READINESS_LOCK};

/// The server-side bound of an operator command (it may wait for a readiness transition).
const OPERATOR_BOUND: Duration = Duration::from_secs(30);
/// The shortest window (the table's `CHECK`).
const MIN_WINDOW: Duration = Duration::from_secs(60);
/// The longest window (the table's `CHECK`).
const MAX_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// A rollout window (design §7.5): while it is open, nodes of exactly two digests may be ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollout {
    id: i64,
    from: Digest,
    to: Digest,
    opened_by: String,
    opened_role: String,
    opened_at: jiff::Timestamp,
    until: jiff::Timestamp,
    closed_at: Option<jiff::Timestamp>,
    active: bool,
}

impl Rollout {
    /// The window's id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The version being replaced.
    pub fn from(&self) -> Digest {
        self.from
    }

    /// The version rolled out.
    pub fn to(&self) -> Digest {
        self.to
    }

    /// Who opened it: the label the opener gave, or the database role when it gave none.
    pub fn opened_by(&self) -> &str {
        &self.opened_by
    }

    /// The database role (`session_user`) that opened it, recorded by the server for audit.
    pub fn opened_role(&self) -> &str {
        &self.opened_role
    }

    /// When it was opened (database time).
    pub fn opened_at(&self) -> jiff::Timestamp {
        self.opened_at
    }

    /// When it ends (database time).
    pub fn until(&self) -> jiff::Timestamp {
        self.until
    }

    /// When it was closed.
    pub fn closed_at(&self) -> Option<jiff::Timestamp> {
        self.closed_at
    }

    /// Whether it allows two versions now: not closed and not ended, as read.
    pub fn is_active(&self) -> bool {
        self.active
    }

    fn from_row(row: &Row, database: &str, what: &'static str) -> Result<Self, NodeError> {
        let time = |us: i64| {
            timestamp(us)
                .ok_or_else(|| NodeError::undecodable(database, what, "a time out of range"))
        };
        let closed: Option<i64> = get(row, 6, database, what)?;
        Ok(Self {
            id: get(row, 0, database, what)?,
            from: digest_col(row, 1, database, what)?,
            to: digest_col(row, 2, database, what)?,
            opened_by: get(row, 3, database, what)?,
            opened_at: time(get(row, 4, database, what)?)?,
            until: time(get(row, 5, database, what)?)?,
            closed_at: closed.map(time).transpose()?,
            active: get(row, 7, database, what)?,
            opened_role: get(row, 8, database, what)?,
        })
    }

    /// Open a window from `from` to `to` for `duration`, in the database of `pool`. `from`
    /// defaults to the single digest of the ready non-dev nodes (fresh, or expired within their
    /// lease: the digest a readiness transition compares with). A window already open is closed
    /// in the same transaction (under the readiness lock). `opened_by` labels the opener
    /// (`None`: the database role); the server records the role itself in any case
    /// ([`opened_role`](Self::opened_role)).
    ///
    /// # Errors
    ///
    /// [`NodeError::RolloutRefused`] when `duration` is outside 1 minute – 24 hours or `from`
    /// equals `to` (both refused before connecting), or when `from` is omitted and the ready
    /// nodes run zero or several digests; [`NodeError::InvalidSetting`] for an empty or
    /// over-long `opened_by` (1–128 bytes); [`NodeError::TableMissing`], [`NodeError::Timeout`],
    /// [`NodeError::Database`].
    pub async fn open(
        pool: &Pool,
        to: Digest,
        from: Option<Digest>,
        duration: Duration,
        opened_by: Option<&str>,
    ) -> Result<Rollout, NodeError> {
        const WHAT: &str = "opening the rollout window";
        if let Some(label) = opened_by
            && (label.is_empty() || label.len() > 128)
        {
            return Err(NodeError::invalid_setting(
                "rollout.opened_by",
                format!("invalid rollout opener {label:?}: 1 to 128 bytes"),
            ));
        }
        // what needs no database is refused before connecting
        let refused_early = |reason: String| NodeError::RolloutRefused {
            database: String::new(),
            reason,
        };
        if !(MIN_WINDOW..=MAX_WINDOW).contains(&duration) || duration.subsec_nanos() != 0 {
            return Err(refused_early(format!(
                "the window must last from 1 minute to 24 hours, got {} s",
                duration.as_secs_f64()
            )));
        }
        if from == Some(to) {
            return Err(refused_early(format!(
                "the window must replace a version: from and to are both {to}"
            )));
        }
        let (conn, deadline) = connect(pool, WHAT).await?;
        let begin = sql::begin_sql(ms(OPERATOR_BOUND), ms(with_grace(OPERATOR_BOUND)), false);
        let (class, key) = READINESS_LOCK;
        let lock_params: [(&(dyn ToSql + Sync), Type); 2] = [
            (&class, sql::LOCK_TYPES[0].clone()),
            (&key, sql::LOCK_TYPES[1].clone()),
        ];
        let answers = {
            let mut flight = Flight::default();
            flight
                .batch(&conn, &begin)
                .query(&conn, sql::CHECK_SQL, &[])
                .query(&conn, sql::LOCK_SQL, &lock_params)
                .query(&conn, sql::READY_DIGESTS_SQL, &[]);
            tokio::time::timeout_at(deadline, flight.send()).await
        };
        let Ok(answers) = answers else {
            // the guard discards the connection: it may still be mid-statement
            return Err(timed_out("", WHAT));
        };
        let (rows, failure) = split(answers);
        let database = match checked_database(rows_at(&rows, 1).first(), WHAT) {
            Ok(database) => database,
            Err(e) => {
                rollback(conn).await;
                return Err(e);
            }
        };
        if let Some(e) = failure {
            rollback(conn).await;
            return Err(NodeError::pg(&database, WHAT, e));
        }
        let refused = |reason: String| NodeError::RolloutRefused {
            database: database.clone(),
            reason,
        };
        let ready: Result<Vec<Digest>, NodeError> = rows_at(&rows, 3)
            .iter()
            .map(|row| digest_col(row, 0, &database, WHAT))
            .collect();
        let ready = match ready {
            Ok(ready) => ready,
            Err(e) => {
                rollback(conn).await;
                return Err(e);
            }
        };
        let from = match (from, ready.as_slice()) {
            (Some(from), _) => from,
            (None, [only]) => *only,
            (None, []) => {
                rollback(conn).await;
                return Err(refused(
                    "no node is ready; name the version being replaced with --from".to_owned(),
                ));
            }
            (None, many) => {
                let listed: Vec<String> = many.iter().map(ToString::to_string).collect();
                rollback(conn).await;
                return Err(refused(format!(
                    "the ready nodes run {} digests ({}); name the version being replaced with \
                     --from",
                    many.len(),
                    listed.join(", ")
                )));
            }
        };
        if from == to {
            rollback(conn).await;
            return Err(refused(format!(
                "the window must replace a version: from and to are both {to}"
            )));
        }
        let duration_ms = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
        let t = &sql::OPEN_TYPES;
        let open_params: [(&(dyn ToSql + Sync), Type); 4] = [
            (&from.as_bytes().as_slice(), t[0].clone()),
            (&to.as_bytes().as_slice(), t[1].clone()),
            (&opened_by, t[2].clone()),
            (&duration_ms, t[3].clone()),
        ];
        let answers = {
            let mut flight = Flight::default();
            flight
                .query(&conn, sql::CLOSE_SQL, &[])
                .query(&conn, sql::OPEN_SQL, &open_params)
                .batch(&conn, "COMMIT");
            tokio::time::timeout_at(deadline, flight.send()).await
        };
        let Ok(answers) = answers else {
            // the guard discards the connection: it may still be mid-statement
            return Err(timed_out(&database, WHAT));
        };
        let (rows, failure) = split(answers);
        if let Some(e) = failure {
            rollback(conn).await;
            return Err(NodeError::pg(&database, WHAT, e));
        }
        conn.recycle();
        if let Some(closed) = rows_at(&rows, 0).first() {
            let closed = Rollout::from_row(closed, &database, WHAT)?;
            tracing::info!(target: "erp::node", database = %database, from = %closed.from,
                to = %closed.to, "the open rollout window was closed by a new one");
        }
        let Some(opened) = rows_at(&rows, 1).first() else {
            return Err(NodeError::undecodable(
                &database,
                WHAT,
                "the insert returned no row",
            ));
        };
        let opened = Rollout::from_row(opened, &database, WHAT)?;
        tracing::info!(target: "erp::node", database = %database, from = %opened.from,
            to = %opened.to, until = %opened.until, opened_by, "rollout window opened");
        Ok(opened)
    }

    /// Close the open window, if any (under the readiness lock).
    ///
    /// # Errors
    ///
    /// [`NodeError::TableMissing`], [`NodeError::Timeout`], [`NodeError::Database`].
    pub async fn close(pool: &Pool) -> Result<Option<Rollout>, NodeError> {
        const WHAT: &str = "closing the rollout window";
        let (conn, deadline) = connect(pool, WHAT).await?;
        let begin = sql::begin_sql(ms(OPERATOR_BOUND), ms(with_grace(OPERATOR_BOUND)), false);
        let (class, key) = READINESS_LOCK;
        let lock_params: [(&(dyn ToSql + Sync), Type); 2] = [
            (&class, sql::LOCK_TYPES[0].clone()),
            (&key, sql::LOCK_TYPES[1].clone()),
        ];
        let answers = {
            let mut flight = Flight::default();
            flight
                .batch(&conn, &begin)
                .query(&conn, sql::CHECK_SQL, &[])
                .query(&conn, sql::LOCK_SQL, &lock_params)
                .query(&conn, sql::CLOSE_SQL, &[])
                .batch(&conn, "COMMIT");
            tokio::time::timeout_at(deadline, flight.send()).await
        };
        let Ok(answers) = answers else {
            // the guard discards the connection: it may still be mid-statement
            return Err(timed_out("", WHAT));
        };
        let (rows, failure) = split(answers);
        let database = match checked_database(rows_at(&rows, 1).first(), WHAT) {
            Ok(database) => database,
            Err(e) => {
                rollback(conn).await;
                return Err(e);
            }
        };
        if let Some(e) = failure {
            rollback(conn).await;
            return Err(NodeError::pg(&database, WHAT, e));
        }
        conn.recycle();
        let closed = rows_at(&rows, 3)
            .first()
            .map(|row| Rollout::from_row(row, &database, WHAT))
            .transpose()?;
        if let Some(c) = &closed {
            tracing::info!(target: "erp::node", database = %database, from = %c.from, to = %c.to,
                "rollout window closed");
        }
        Ok(closed)
    }

    /// The window not closed yet (check [`is_active`](Self::is_active): it may have ended).
    ///
    /// # Errors
    ///
    /// [`NodeError::TableMissing`], [`NodeError::Timeout`], [`NodeError::Database`].
    pub async fn current(pool: &Pool) -> Result<Option<Rollout>, NodeError> {
        const WHAT: &str = "reading the rollout window";
        let (database, rows) = read(pool, WHAT, sql::CURRENT_SQL).await?;
        rows.first()
            .map(|row| Rollout::from_row(row, &database, WHAT))
            .transpose()
    }
}

/// One `erp_node` row, for `erp cluster nodes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    id: NodeId,
    name: String,
    host: String,
    pid: i32,
    role: NodeRole,
    dev: bool,
    digest: Digest,
    config_digest: Digest,
    build_digest: Digest,
    addons_digest: Digest,
    build_label: String,
    registered_at: jiff::Timestamp,
    state: NodeState,
    unready_reason: Option<String>,
    heartbeat_at: jiff::Timestamp,
    expires_at: jiff::Timestamp,
    beat: i64,
    fresh: bool,
}

impl NodeRow {
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

    /// The role.
    pub fn role(&self) -> NodeRole {
        self.role
    }

    /// Whether it is an `erp dev` node.
    pub fn is_dev(&self) -> bool {
        self.dev
    }

    /// The combined digest.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The configuration digest.
    pub fn config_digest(&self) -> Digest {
        self.config_digest
    }

    /// The build digest.
    pub fn build_digest(&self) -> Digest {
        self.build_digest
    }

    /// The addons digest.
    pub fn addons_digest(&self) -> Digest {
        self.addons_digest
    }

    /// The build label.
    pub fn build_label(&self) -> &str {
        &self.build_label
    }

    /// When it registered (database time).
    pub fn registered_at(&self) -> jiff::Timestamp {
        self.registered_at
    }

    /// Its state.
    pub fn state(&self) -> NodeState {
        self.state
    }

    /// Why it is unready (`digest_mismatch`, `two_versions_live`).
    pub fn unready_reason(&self) -> Option<&str> {
        self.unready_reason.as_deref()
    }

    /// Its last renewal (database time).
    pub fn heartbeat_at(&self) -> jiff::Timestamp {
        self.heartbeat_at
    }

    /// When its lease ends (database time).
    pub fn expires_at(&self) -> jiff::Timestamp {
        self.expires_at
    }

    /// Its beat.
    pub fn beat(&self) -> i64 {
        self.beat
    }

    /// Whether its lease had not ended when it was read.
    pub fn is_fresh(&self) -> bool {
        self.fresh
    }

    fn from_row(row: &Row, database: &str, what: &'static str) -> Result<Self, NodeError> {
        let time = |us: i64| {
            timestamp(us)
                .ok_or_else(|| NodeError::undecodable(database, what, "a time out of range"))
        };
        let id: &str = get(row, 0, database, what)?;
        let role: &str = get(row, 4, database, what)?;
        let state: &str = get(row, 12, database, what)?;
        Ok(Self {
            id: NodeId::parse(id).ok_or_else(|| NodeError::undecodable(database, what, id))?,
            name: get(row, 1, database, what)?,
            host: get(row, 2, database, what)?,
            pid: get(row, 3, database, what)?,
            role: NodeRole::parse(role)
                .ok_or_else(|| NodeError::undecodable(database, what, role))?,
            dev: get(row, 5, database, what)?,
            digest: digest_col(row, 6, database, what)?,
            config_digest: digest_col(row, 7, database, what)?,
            build_digest: digest_col(row, 8, database, what)?,
            addons_digest: digest_col(row, 9, database, what)?,
            build_label: get(row, 10, database, what)?,
            registered_at: time(get(row, 11, database, what)?)?,
            state: NodeState::parse(state)
                .ok_or_else(|| NodeError::undecodable(database, what, state))?,
            unready_reason: get(row, 13, database, what)?,
            heartbeat_at: time(get(row, 14, database, what)?)?,
            expires_at: time(get(row, 15, database, what)?)?,
            beat: get(row, 16, database, what)?,
            fresh: get(row, 17, database, what)?,
        })
    }
}

/// Every row of `erp_node` in the database of `pool`, oldest registration first.
///
/// # Errors
///
/// [`NodeError::TableMissing`], [`NodeError::Timeout`], [`NodeError::Database`].
pub async fn list_nodes(pool: &Pool) -> Result<Vec<NodeRow>, NodeError> {
    const WHAT: &str = "listing the nodes";
    let (database, rows) = read(pool, WHAT, sql::LIST_SQL).await?;
    rows.iter()
        .map(|row| NodeRow::from_row(row, &database, WHAT))
        .collect()
}

async fn connect(
    pool: &Pool,
    what: &'static str,
) -> Result<(Conn, tokio::time::Instant), NodeError> {
    let after = with_grace(OPERATOR_BOUND);
    let deadline = tokio::time::Instant::now() + after;
    match tokio::time::timeout_at(deadline, pool.get()).await {
        Ok(Ok(conn)) => Ok((Conn::new(conn), deadline)),
        Ok(Err(e)) => Err(NodeError::pool("", what, e)),
        Err(_) => Err(NodeError::Timeout {
            database: String::new(),
            what,
            after,
        }),
    }
}

fn timed_out(database: &str, what: &'static str) -> NodeError {
    NodeError::Timeout {
        database: database.to_owned(),
        what,
        after: with_grace(OPERATOR_BOUND),
    }
}

/// A read-only statement after the table check, in one bounded transaction.
async fn read(
    pool: &Pool,
    what: &'static str,
    statement: &'static str,
) -> Result<(String, Vec<Row>), NodeError> {
    let (conn, deadline) = connect(pool, what).await?;
    let begin = sql::begin_sql(ms(OPERATOR_BOUND), ms(with_grace(OPERATOR_BOUND)), false);
    let answers = {
        let mut flight = Flight::default();
        flight
            .batch(&conn, &begin)
            .query(&conn, sql::CHECK_SQL, &[])
            .query(&conn, statement, &[])
            .batch(&conn, "COMMIT");
        tokio::time::timeout_at(deadline, flight.send()).await
    };
    let Ok(answers) = answers else {
        // the guard discards the connection: it may still be mid-statement
        return Err(timed_out("", what));
    };
    let (mut rows, failure) = split(answers);
    let database = match checked_database(rows_at(&rows, 1).first(), what) {
        Ok(database) => database,
        Err(e) => {
            rollback(conn).await;
            return Err(e);
        }
    };
    if let Some(e) = failure {
        rollback(conn).await;
        return Err(NodeError::pg(&database, what, e));
    }
    conn.recycle();
    let rows = if rows.len() > 2 {
        rows.swap_remove(2)
    } else {
        Vec::new()
    };
    Ok((database, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_bounds_match_the_table_checks() {
        assert!(super::super::NODE_DDL.contains("until >= opened_at + interval '1 minute'"));
        assert!(super::super::NODE_DDL.contains("until <= opened_at + interval '24 hours'"));
        assert_eq!(MIN_WINDOW, Duration::from_secs(60));
        assert_eq!(MAX_WINDOW, Duration::from_secs(86_400));
    }
}
