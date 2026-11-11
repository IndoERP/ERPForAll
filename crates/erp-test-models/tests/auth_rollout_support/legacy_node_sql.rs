//! Frozen ERPForAll historical SQL constants, extracted by the task evidence script.
//! Source: 70eb59f3ab5bfb709ba8a8e7894df0366890063b:crates/erp-orm/src/node/sql.rs. SQL emulation, not an old executable.
use tokio_postgres::types::Type;

macro_rules! us {
    ($e:literal) => {
        concat!("(extract(epoch FROM ", $e, ") * 1000000)::int8")
    };
}

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

/// Registration: insert the row and its details.
pub(crate) const REGISTER_SQL: &str = concat!(
    "WITH node AS (
    INSERT INTO erp_node (id, name, host, pid, role, dev, digest, config_digest, build_digest,
                          addons_digest, build_label, registered_at, state, unready_reason,
                          heartbeat_at, expires_at, beat, interval_ms, lease_ms)
    VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now(), 'starting', NULL,
            now(), now() + $12 * interval '1 millisecond', 1, $15, $12)
    RETURNING id
), detail AS (
    INSERT INTO erp_node_detail (node_id, kind, payload)
    SELECT node.id, d.kind, d.payload FROM node, unnest($13::text[], $14::bytea[]) AS d (kind, payload)
)
SELECT ",
    us!("now()"),
    " FROM node"
);
pub(crate) const REGISTER_TYPES: [Type; 15] = [
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
];

/// The renewal's upsert, fenced by the beat (`$15` lease ms, `$16` beat sent last, `$17`
/// interval ms).
macro_rules! renew_upsert {
    () => {
        "me AS (
    INSERT INTO erp_node AS n (id, name, host, pid, role, dev, digest, config_digest, build_digest,
                               addons_digest, build_label, registered_at, state, unready_reason,
                               heartbeat_at, expires_at, beat, interval_ms, lease_ms)
    VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
            'epoch'::timestamptz + $12 * interval '1 microsecond', $13, $14,
            now(), now() + $15 * interval '1 millisecond', $16 + 1, $17, $15)
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
pub(crate) const RENEW_TYPES: [Type; 17] = [
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
];
