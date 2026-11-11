//! The registry's statements and the pipelined flight that sends them (design §6.2, §7.2).
//!
//! Database times travel as microseconds since the Unix epoch (`int8`): exact both ways, with no
//! dependence on the session's `DateStyle` or `TimeZone`.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use tokio_postgres::Row;
use tokio_postgres::types::{ToSql, Type};

/// `now()` as microseconds since the epoch.
macro_rules! us {
    ($e:literal) => {
        concat!("(extract(epoch FROM ", $e, ") * 1000000)::int8")
    };
}

/// Which tables exist, and the database's name (first statement of a registration).
pub(crate) const CHECK_SQL: &str = "SELECT current_database(), \
     to_regclass('erp_node') IS NOT NULL, to_regclass('erp_node_detail') IS NOT NULL, \
     to_regclass('erp_node_rollout') IS NOT NULL";

/// The readiness lock (`$1`, `$2` int4).
pub(crate) const LOCK_SQL: &str = "SELECT pg_advisory_xact_lock($1, $2)";
pub(crate) const LOCK_TYPES: [Type; 2] = [Type::INT4, Type::INT4];

/// Registration: this id's own row (state, beat, fresh), and the live non-dev nodes (for a dev
/// identity).
pub(crate) const REGISTER_READ_SQL: &str = "\
SELECT (SELECT state FROM erp_node WHERE id = $1::uuid),
       (SELECT beat FROM erp_node WHERE id = $1::uuid),
       (SELECT expires_at > now() FROM erp_node WHERE id = $1::uuid),
       ARRAY(SELECT format('%s (host %s, pid %s, %s)', name, host, pid, role)
               FROM erp_node
              WHERE expires_at > now() AND state <> 'stopped' AND NOT dev AND id <> $1::uuid
              ORDER BY name, host, pid)";
pub(crate) const ID_TYPES: [Type; 1] = [Type::TEXT];

/// Registration: drop this id's own `stopped` or expired row (a re-registration after a shutdown
/// or a crash; its details go with it).
pub(crate) const DELETE_OWN_SQL: &str =
    "DELETE FROM erp_node WHERE id = $1::uuid AND (state = 'stopped' OR expires_at <= now())";

/// Registration: adopt this process's own committed `starting` row at beat 1 (a registration
/// whose commit answer was lost): its lease starts again now (`$2` lease ms).
pub(crate) const ADOPT_SQL: &str = concat!(
    "UPDATE erp_node SET heartbeat_at = now(), expires_at = now() + $2 * interval '1 millisecond' \
     WHERE id = $1::uuid AND beat = 1 AND state = 'starting' AND expires_at > now() RETURNING ",
    us!("registered_at"),
    ", ",
    us!("now()")
);
pub(crate) const ID_LEASE_TYPES: [Type; 2] = [Type::TEXT, Type::INT8];

/// Housekeeping (design §4.2, §6.3), first transaction: dead node rows past the retention (`$1`
/// own id, `$2` retention ms), at most 64 per statement, never waiting on a locked node row; their
/// details go by the cascade. Sent after the round's view is published, in a transaction of its
/// own with a short `lock_timeout` ([`purge_begin_sql`]), never as part of a registration or a
/// renewal.
///
/// Column 0: node rows deleted.
pub(crate) const PURGE_NODES_SQL: &str = "\
WITH purged AS (
    DELETE FROM erp_node
     WHERE id IN (SELECT id FROM erp_node
                   WHERE expires_at <= now() - $2 * interval '1 millisecond' AND id <> $1::uuid
                   ORDER BY expires_at LIMIT 64 FOR UPDATE SKIP LOCKED)
    RETURNING 1
)
SELECT count(*) FROM purged";
pub(crate) const PURGE_NODES_TYPES: [Type; 2] = [Type::TEXT, Type::INT8];

/// Housekeeping, second transaction: closed or ended rollout windows past the retention (`$1`
/// retention ms), at most 64. Without a row-locking clause: PostgreSQL requires `UPDATE` for one,
/// and the heartbeat role may only read and delete windows (opening and closing them stays with the
/// operator, design §4.3). A window an operator's open transaction holds makes the statement wait
/// up to its `lock_timeout`; the purge then leaves the windows for its next turn, and the dead node
/// rows of the first transaction are already gone. The age is checked again on the row being
/// deleted: after such a wait PostgreSQL re-evaluates only this outer condition on the row's new
/// version, so a window extended or reopened meanwhile is kept.
///
/// Column 0: windows deleted.
pub(crate) const PURGE_WINDOWS_SQL: &str = "\
WITH purged_windows AS (
    DELETE FROM erp_node_rollout
     WHERE id IN (SELECT id FROM erp_node_rollout
                   WHERE coalesce(closed_at, until) <= now() - $1 * interval '1 millisecond'
                   ORDER BY id LIMIT 64)
       AND coalesce(closed_at, until) <= now() - $1 * interval '1 millisecond'
    RETURNING 1
)
SELECT count(*) FROM purged_windows";
pub(crate) const PURGE_WINDOWS_TYPES: [Type; 1] = [Type::INT8];

/// Registration: insert the row and its details.
pub(crate) const REGISTER_SQL: &str = concat!(
    "WITH node AS (
    INSERT INTO erp_node (id, name, host, pid, role, dev, digest, config_digest, build_digest,
                          addons_digest, build_label, registered_at, state, unready_reason,
                          heartbeat_at, expires_at, beat, interval_ms, lease_ms, capabilities)
    VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now(), 'starting', NULL,
            now(), now() + $12 * interval '1 millisecond', 1, $15, $12, $16)
    RETURNING id
), detail AS (
    INSERT INTO erp_node_detail (node_id, kind, payload)
    SELECT node.id, d.kind, d.payload FROM node, unnest($13::text[], $14::bytea[]) AS d (kind, payload)
)
SELECT ",
    us!("now()"),
    " FROM node"
);
pub(crate) const REGISTER_TYPES: [Type; 16] = [
    Type::TEXT,
    Type::TEXT,
    Type::TEXT,
    Type::INT4,
    Type::TEXT,
    Type::BOOL,
    Type::BYTEA,
    Type::BYTEA,
    Type::BYTEA,
    Type::BYTEA,
    Type::TEXT,
    Type::INT8,
    Type::TEXT_ARRAY,
    Type::BYTEA_ARRAY,
    Type::INT8,
    Type::JSONB,
];

/// The renewal's upsert, fenced by the beat (`$15` lease ms, `$16` beat sent last, `$17`
/// interval ms, `$18` immutable capability map).
macro_rules! renew_upsert {
    () => {
        "me AS (
    INSERT INTO erp_node AS n (id, name, host, pid, role, dev, digest, config_digest, build_digest,
                               addons_digest, build_label, registered_at, state, unready_reason,
                               heartbeat_at, expires_at, beat, interval_ms, lease_ms, capabilities)
    VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
            'epoch'::timestamptz + $12 * interval '1 microsecond', $13, $14,
            now(), now() + $15 * interval '1 millisecond', $16 + 1, $17, $15, $18)
    ON CONFLICT (id) DO UPDATE
       SET state = EXCLUDED.state, unready_reason = EXCLUDED.unready_reason,
           heartbeat_at = EXCLUDED.heartbeat_at, expires_at = EXCLUDED.expires_at,
           beat = EXCLUDED.beat
     WHERE n.beat <= $16
    RETURNING old.id IS NULL AS inserted
)"
    };
}

/// What the renewal reads in the same statement: the database time, the outcome, the open window
/// and the peers (fresh, and expired within their own lease; not `stopped`). One row per peer, or
/// one row with `NULL` peer columns.
macro_rules! renew_select {
    () => {
        concat!(
            "
SELECT ",
            us!("now()"),
            ", (SELECT inserted FROM me), w.from_digest, w.to_digest, ",
            us!("w.until"),
            ",
       p.id::text, p.name, p.role, p.state, p.digest, p.expires_at > now()
  FROM (SELECT 1) AS one
  LEFT JOIN LATERAL (SELECT from_digest, to_digest, until FROM erp_node_rollout
                      WHERE closed_at IS NULL AND until > now()
                      ORDER BY id DESC LIMIT 1) AS w ON true
  LEFT JOIN LATERAL (SELECT id, name, role, state, digest, expires_at FROM erp_node
                      WHERE id <> $1::uuid AND state <> 'stopped'
                        AND expires_at > now() - lease_ms * interval '1 millisecond'
                      ORDER BY id) AS p ON true"
        )
    };
}

/// The renewal (design §6.2): one upsert fenced by the beat, and in the same statement what it
/// reads. It names no table it does not need (the purge runs in transactions of its own): a
/// statement takes its table locks whatever its predicates say.
///
/// Columns: 0 `db_now` µs, 1 `inserted` (`NULL`: nothing written), 2 window from, 3 window to,
/// 4 window until µs, 5 peer id, 6 name, 7 role, 8 state, 9 digest, 10 fresh.
pub(crate) const RENEW_SQL: &str = concat!("WITH ", renew_upsert!(), renew_select!());
pub(crate) const RENEW_TYPES: [Type; 18] = [
    Type::TEXT,
    Type::TEXT,
    Type::TEXT,
    Type::INT4,
    Type::TEXT,
    Type::BOOL,
    Type::BYTEA,
    Type::BYTEA,
    Type::BYTEA,
    Type::BYTEA,
    Type::TEXT,
    Type::INT8,
    Type::TEXT,
    Type::TEXT,
    Type::INT8,
    Type::INT8,
    Type::INT8,
    Type::JSONB,
];

/// The database time and the open window (one row always).
pub(crate) const WINDOW_SQL: &str = concat!(
    "SELECT ",
    us!("now()"),
    ", w.from_digest, w.to_digest, ",
    us!("w.until"),
    "
  FROM (SELECT 1) AS one
  LEFT JOIN LATERAL (SELECT from_digest, to_digest, until FROM erp_node_rollout
                      WHERE closed_at IS NULL AND until > now()
                      ORDER BY id DESC LIMIT 1) AS w ON true"
);

/// Peers, fresh and expired within their own lease, not `stopped` (`$1` own id).
///
/// Columns: 0 id, 1 name, 2 role, 3 state, 4 digest, 5 config, 6 build, 7 addons, 8 build label,
/// 9 fresh, 10 dev, 11 interval ms, 12 host, 13 pid.
pub(crate) const PEERS_SQL: &str = "\
SELECT id::text, name, role, state, digest, config_digest, build_digest, addons_digest, build_label,
       expires_at > now(), dev, interval_ms, host, pid
  FROM erp_node
 WHERE id <> $1::uuid AND state <> 'stopped'
   AND expires_at > now() - lease_ms * interval '1 millisecond'
 ORDER BY id";

/// This node's own row: beat, state, expiry.
pub(crate) const OWN_SQL: &str = concat!(
    "SELECT beat, state, ",
    us!("expires_at"),
    " FROM erp_node WHERE id = $1::uuid"
);

/// The details of a row the renewal had to re-insert (`$1` id, `$2` kinds, `$3` payloads).
pub(crate) const DETAIL_INSERT_SQL: &str = "\
INSERT INTO erp_node_detail (node_id, kind, payload)
SELECT $1::uuid, d.kind, d.payload FROM unnest($2::text[], $3::bytea[]) AS d (kind, payload)
ON CONFLICT (node_id, kind) DO UPDATE SET payload = EXCLUDED.payload";
pub(crate) const DETAIL_INSERT_TYPES: [Type; 3] = [Type::TEXT, Type::TEXT_ARRAY, Type::BYTEA_ARRAY];

/// The detail payloads of some nodes (a mismatch report).
pub(crate) const DETAIL_SQL: &str =
    "SELECT node_id::text, kind, payload FROM erp_node_detail WHERE node_id = ANY($1::uuid[])";
pub(crate) const DETAIL_TYPES: [Type; 1] = [Type::TEXT_ARRAY];

/// A readiness decision, fenced by the beat.
pub(crate) const TRANSITION_WRITE_SQL: &str = "UPDATE erp_node SET state = $3, unready_reason = $4 WHERE id = $1::uuid AND beat <= $2 \
     RETURNING beat";
pub(crate) const TRANSITION_WRITE_TYPES: [Type; 4] =
    [Type::TEXT, Type::INT8, Type::TEXT, Type::TEXT];

/// The shutdown: state `stopped`, lease ended now. [`OWN_SQL`] follows it in the same flight, to
/// tell a row that is gone from one that is ahead of this process (a clone renews it).
pub(crate) const SHUTDOWN_SQL: &str = "\
UPDATE erp_node SET state = 'stopped', unready_reason = NULL, heartbeat_at = now(),
       expires_at = now(), beat = $2 + 1
 WHERE id = $1::uuid AND beat <= $2
RETURNING beat";
pub(crate) const ID_BEAT_TYPES: [Type; 2] = [Type::TEXT, Type::INT8];

/// Rollout columns: 0 id, 1 from, 2 to, 3 opened_by, 4 opened_at µs, 5 until µs, 6 closed_at µs,
/// 7 active, 8 opened_role.
macro_rules! rollout_cols {
    () => {
        concat!(
            "id, from_digest, to_digest, opened_by, ",
            us!("opened_at"),
            ", ",
            us!("until"),
            ", ",
            us!("closed_at"),
            ", closed_at IS NULL AND until > now(), opened_role"
        )
    };
}

/// Digests of the ready non-dev nodes, fresh or expired within their own lease: the reference set
/// of a transition (design §7.2), so the default `from` of a window is what transitions compare
/// with.
pub(crate) const READY_DIGESTS_SQL: &str = "SELECT DISTINCT digest FROM erp_node \
     WHERE state = 'ready' AND NOT dev AND expires_at > now() - lease_ms * interval '1 millisecond' \
     ORDER BY digest";

/// Close the open window.
pub(crate) const CLOSE_SQL: &str = concat!(
    "UPDATE erp_node_rollout SET closed_at = now() WHERE closed_at IS NULL RETURNING ",
    rollout_cols!()
);

/// Open a window (`$1` from, `$2` to, `$3` opened by or `NULL` for the session role, `$4`
/// duration ms). `opened_role` is the column default (`session_user`), never sent.
pub(crate) const OPEN_SQL: &str = concat!(
    "INSERT INTO erp_node_rollout (from_digest, to_digest, opened_by, until) \
     VALUES ($1, $2, coalesce($3, session_user::text), now() + $4 * interval '1 millisecond') \
     RETURNING ",
    rollout_cols!()
);
pub(crate) const OPEN_TYPES: [Type; 4] = [Type::BYTEA, Type::BYTEA, Type::TEXT, Type::INT8];

/// The window not closed yet (it may have ended).
pub(crate) const CURRENT_SQL: &str = concat!(
    "SELECT ",
    rollout_cols!(),
    " FROM erp_node_rollout WHERE closed_at IS NULL"
);

/// Every row, for `erp cluster nodes`.
///
/// Columns: 0 id, 1 name, 2 host, 3 pid, 4 role, 5 dev, 6 digest, 7 config, 8 build, 9 addons,
/// 10 label, 11 registered µs, 12 state, 13 reason, 14 heartbeat µs, 15 expires µs, 16 beat,
/// 17 fresh.
pub(crate) const LIST_SQL: &str = concat!(
    "SELECT id::text, name, host, pid, role, dev, digest, config_digest, build_digest, \
     addons_digest, build_label, ",
    us!("registered_at"),
    ", state, unready_reason, ",
    us!("heartbeat_at"),
    ", ",
    us!("expires_at"),
    ", beat, expires_at > now() FROM erp_node ORDER BY registered_at, id"
);

/// The answer of one request of a flight.
pub(crate) type Answer = Result<Vec<Row>, tokio_postgres::Error>;

type Request<'a> = Pin<Box<dyn Future<Output = Answer> + Send + 'a>>;

/// The requests of one flight: polled together, in order, so they leave back to back and are
/// answered in one round trip (the first poll of each puts it on the wire).
#[derive(Default)]
pub(crate) struct Flight<'a> {
    requests: Vec<Request<'a>>,
}

impl<'a> Flight<'a> {
    /// A simple-protocol batch (`BEGIN …`, `COMMIT`).
    pub(crate) fn batch(&mut self, client: &'a tokio_postgres::Client, sql: &'a str) -> &mut Self {
        self.requests.push(Box::pin(async move {
            client.batch_execute(sql).await.map(|()| Vec::new())
        }));
        self
    }

    /// An unnamed statement with typed parameters.
    pub(crate) fn query(
        &mut self,
        client: &'a tokio_postgres::Client,
        sql: &'a str,
        params: &'a [(&'a (dyn ToSql + Sync), Type)],
    ) -> &mut Self {
        self.requests.push(Box::pin(
            async move { client.query_typed(sql, params).await },
        ));
        self
    }

    /// Send everything and collect the answers in order.
    pub(crate) async fn send(self) -> Vec<Answer> {
        let mut requests = self.requests;
        let mut answers: Vec<Option<Answer>> = (0..requests.len()).map(|_| None).collect();
        std::future::poll_fn(|cx| {
            let mut pending = false;
            for (request, answer) in requests.iter_mut().zip(answers.iter_mut()) {
                if answer.is_none() {
                    match request.as_mut().poll(cx) {
                        Poll::Ready(r) => *answer = Some(r),
                        Poll::Pending => pending = true,
                    }
                }
            }
            if pending {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        answers.into_iter().flatten().collect()
    }
}

/// `BEGIN` with the limits of a registry transaction: each statement within `statement_ms`, the
/// whole transaction and every idle gap in it within `transaction_ms` (PostgreSQL ends the
/// session past either, so a stopped client never holds the readiness lock; PostgreSQL 17+, the
/// floor is 18), and its commit mode. `transaction_timeout` is set to 0 first:
/// PostgreSQL arms its timer at the transaction's start from the session value, and a positive
/// value set while a timer runs does not re-arm it (`pg-features.md` §3.5), so a longer session
/// or role backstop would otherwise win.
///
/// A renewal commits asynchronously (`async_commit`). Every other registry transaction sets
/// `synchronous_commit = on` itself: the self-fence treats its commit as durable when answered
/// (design §6.4), so a database or role default of `off` or `local` must not apply to it. `on`
/// also waits for a synchronous standby where one is configured (`local` would not, and a
/// failover may promote that standby); `remote_apply` adds only visibility on the standby, which
/// the deadline does not need.
/// The `BEGIN` batch of a purge transaction: [`begin_sql`] with an asynchronous commit (a lost
/// purge only runs again) and a `lock_timeout` of `lock_ms`, so a row an operator's transaction
/// holds costs the purge at most that long (55P03, `lock_not_available`).
pub(crate) fn purge_begin_sql(statement_ms: u64, transaction_ms: u64, lock_ms: u64) -> String {
    format!(
        "{}; SET LOCAL lock_timeout = {lock_ms}",
        begin_sql(statement_ms, transaction_ms, true)
    )
}

pub(crate) fn begin_sql(statement_ms: u64, transaction_ms: u64, async_commit: bool) -> String {
    let commit = if async_commit { "off" } else { "on" };
    format!(
        "BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL statement_timeout = {statement_ms}; \
         SET LOCAL idle_in_transaction_session_timeout = {transaction_ms}; \
         SET LOCAL transaction_timeout = 0; \
         SET LOCAL transaction_timeout = {transaction_ms}; \
         SET LOCAL synchronous_commit = {commit}"
    )
}
