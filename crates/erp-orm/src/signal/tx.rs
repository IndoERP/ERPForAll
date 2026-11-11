//! The transaction side of the signals: install, bump, compact, payload-free notify.

use std::time::Duration;

use erp_fields::{Error, Result};

use super::{Generations, NotifyChannel, SignalKey, undecodable};
use crate::Transaction;

/// Whether the process that bumps a key has already applied the change in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SelfApplied {
    /// This process applied its own change: after the commit it does not refresh for it.
    Yes,
    /// It did not: the next transaction refreshes.
    No,
}

#[derive(Debug)]
enum What {
    Notify(NotifyChannel),
    Bump(SignalKey, SelfApplied),
}

#[derive(Debug)]
struct Sent {
    /// Number of open savepoints when it was sent.
    depth: usize,
    what: What,
}

/// What a transaction sent for the signals, per savepoint depth, and the generations it began at.
#[derive(Debug, Default)]
pub(crate) struct TxSignal {
    began_at: Option<Generations>,
    /// [`Signal::full_refreshes`](super::Signal::full_refreshes) loaded before the read that
    /// produced `began_at` was sent.
    began_full_refreshes: u64,
    sent: Vec<Sent>,
}

impl TxSignal {
    /// The transaction's snapshot read `generations`; `full_refreshes` was loaded before that
    /// read was sent.
    pub(crate) fn began(&mut self, generations: Generations, full_refreshes: u64) {
        self.began_at = Some(generations);
        self.began_full_refreshes = full_refreshes;
        self.sent.clear();
    }

    /// The generations the transaction's snapshot read at `BEGIN` (`None` before it opened, or
    /// when it opened without the read).
    pub(crate) fn began_at(&self) -> Option<&Generations> {
        self.began_at.as_ref()
    }

    /// See [`began`](Self::began) (meaningful only while [`began_at`](Self::began_at) is `Some`).
    pub(crate) fn began_full_refreshes(&self) -> u64 {
        self.began_full_refreshes
    }

    /// `ROLLBACK TO SAVEPOINT` left `depth` savepoints open: what was sent deeper is gone.
    pub(crate) fn rolled_back_to(&mut self, depth: usize) {
        self.sent.retain(|s| s.depth <= depth);
    }

    /// `RELEASE SAVEPOINT` left `depth` savepoints open: what was sent deeper now belongs to the
    /// enclosing level (a later rollback of a sibling savepoint must not take it back).
    pub(crate) fn released_to(&mut self, depth: usize) {
        for s in &mut self.sent {
            s.depth = s.depth.min(depth);
        }
    }

    pub(crate) fn reset(&mut self) {
        self.began_at = None;
        self.sent.clear();
    }

    /// The bumps to credit to this process after a successful commit, with the generation each
    /// key had when the transaction began.
    pub(crate) fn take_own_bumps(&mut self) -> Vec<(SignalKey, u64)> {
        let began = self.began_at.take();
        let sent = std::mem::take(&mut self.sent);
        let Some(began) = began else {
            return Vec::new();
        };
        let mut own = Vec::new();
        for s in sent {
            match s.what {
                What::Bump(key, SelfApplied::Yes) => {
                    let at = began.get(&key);
                    own.push((key, at));
                }
                // another process (or nobody) applies these; a notification is no change
                What::Bump(_, SelfApplied::No) | What::Notify(_) => {}
            }
        }
        own
    }

    fn notified(&self, channel: &NotifyChannel) -> bool {
        self.sent
            .iter()
            .any(|s| matches!(&s.what, What::Notify(c) if c == channel))
    }

    fn bumped(&self, key: &SignalKey) -> Option<usize> {
        self.sent
            .iter()
            .position(|s| matches!(&s.what, What::Bump(k, _) if k == key))
    }
}

/// The pattern of the `key` column's `CHECK`, shared by the install and the upgrade DDL.
macro_rules! key_pattern {
    () => {
        "^(registry|policy|membership|cache:[a-z0-9_.]{1,64})$"
    };
}

/// `MAX_SIGNAL_WEIGHT` is spelled out in the DDL (a constant in a `CHECK` cannot be bound); the
/// unit test `install_sql_bounds_the_weight` keeps the two equal.
pub(crate) const INSTALL_SQL: &str = concat!(
    "\
CREATE TABLE IF NOT EXISTS erp_signal_generation (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    key text NOT NULL CHECK (key ~ '",
    key_pattern!(),
    "'),
    weight int8 NOT NULL DEFAULT 1 CHECK (weight BETWEEN 1 AND 1099511627776),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS erp_signal_generation_key
    ON erp_signal_generation (key) INCLUDE (weight)"
);

/// A catalog constraint: definition, comment, `type:columns`.
pub(crate) type ConstraintRow = (String, Option<String>, String);

/// The statements that bring the `key` checks of a signal table made by an older version up to
/// date (before the `membership` key existed, its `CHECK` refused `membership`, so
/// the membership triggers would fail every write they fire on). `constraints` is the catalog's
/// view of the table (name -> definition, comment, `type:columns`). The table carries no
/// version: a `CHECK` on the `key` column alone whose definition is not exactly the current
/// [`key_pattern!`] check as PostgreSQL renders it (`pg_get_constraintdef`) is stale and is
/// dropped (all of them, however many), then the current check is
/// added `NOT VALID` and validated unless a current one already stays. An up-to-date table gives
/// no statement. Rows and generations are untouched. The caller (the schema sync) holds the
/// schema lock, with the schema `lock_timeout` ([`crate::SCHEMA_LOCK_TIMEOUT`], set before the
/// lock is taken); the drop-and-add takes `ACCESS EXCLUSIVE` on the table
/// until the sync commits, which is short (a compacted table of a few rows) and bounded by that
/// timeout; it shares the sync's transaction because the new check must be in place before the
/// membership triggers of the same sync are installed.
pub(crate) fn upgrade_statements(
    constraints: Option<&rustc_hash::FxHashMap<String, ConstraintRow>>,
) -> Vec<String> {
    let Some(constraints) = constraints else {
        return Vec::new();
    };
    let mut stale: Vec<&String> = Vec::new();
    let mut current = false;
    for (name, (definition, _, kind)) in constraints {
        if kind != "c:key" {
            continue;
        }
        if definition == concat!("CHECK ((key ~ '", key_pattern!(), "'::text))") {
            current = true;
        } else {
            stale.push(name);
        }
    }
    if stale.is_empty() {
        return Vec::new();
    }
    stale.sort();
    let mut out: Vec<String> = stale
        .into_iter()
        .map(|name| {
            format!(
                "ALTER TABLE {} DROP CONSTRAINT \"{}\"",
                super::SIGNAL_TABLE,
                name.replace('"', "\"\"")
            )
        })
        .collect();
    if !current {
        out.push(
            concat!(
                "ALTER TABLE erp_signal_generation ADD CONSTRAINT erp_signal_generation_key_check \
             CHECK (key ~ '",
                key_pattern!(),
                "') NOT VALID"
            )
            .to_owned(),
        );
        out.push(
            "ALTER TABLE erp_signal_generation VALIDATE CONSTRAINT erp_signal_generation_key_check"
                .to_owned(),
        );
    }
    out
}

/// The request role's grant on the signal table (the checks are shared, `crate::grantee`).
const SIGNAL_GRANT: crate::grantee::Grant = crate::grantee::Grant {
    call: "Transaction::grant_signal_request_role",
    tables: &["erp_signal_generation"],
    grantee: "the request role",
    missing: |_| {
        "the signal table erp_signal_generation is missing: run Transaction::install_signal \
         before granting the request role"
            .to_owned()
    },
    owner: |role, table| {
        format!(
            "role {role:?} owns {table}: the request role must be another role (revoking from \
             the owner would remove what installation and compaction need)"
        )
    },
};

const BUMP_SQL: &str = "INSERT INTO erp_signal_generation (key) VALUES ($1)";

const COMPACT_SQL: &str = "\
WITH old AS (
    SELECT id, weight FROM erp_signal_generation
    WHERE key = $1 AND created_at < now() - make_interval(secs => $2::float8)
    FOR UPDATE
), n AS (
    SELECT count(*) AS c, COALESCE(sum(weight), 0)::int8 AS s FROM old
), del AS (
    DELETE FROM erp_signal_generation
    WHERE id IN (SELECT id FROM old) AND (SELECT c FROM n) >= 2
    RETURNING 1
), ins AS (
    INSERT INTO erp_signal_generation (key, weight) SELECT $1, s FROM n WHERE c >= 2
    RETURNING 1
)
SELECT (SELECT count(*) FROM del)::int8";

const NOTIFY_SQL: &str = "SELECT pg_notify($1, '')";

impl Transaction {
    /// Create the signal table and its index when missing. A table made by an older version is
    /// brought up to date by the schema sync ([`Orm::sync_schema`](crate::Orm::sync_schema), under
    /// its schema lock), whether or not the `Orm` has a [`Signal`](super::Signal), not here.
    /// `sync_schema` calls this when the `Orm` has a signal.
    ///
    /// # Errors
    ///
    /// `Database` when the DDL fails (the role lacks `CREATE`, or PostgreSQL is older than 18 and
    /// has no `uuidv7()`).
    pub async fn install_signal(&mut self) -> Result<()> {
        self.batch(INSTALL_SQL).await
    }

    /// The state of a transaction that creates the signal table itself (the module system's
    /// database initialization, `Orm::sync_schema`): it opens without reading the generations,
    /// since the table may not exist yet; when the ORM has a [`Signal`](super::Signal) the table
    /// is created and the generations read, so the bumps this transaction makes are credited to
    /// this process after the commit. Call it first, before any other statement.
    ///
    /// # Errors
    ///
    /// `Access` unless the transaction acts as the `System` principal with full elevation; the
    /// errors of [`Transaction::install_signal`] and [`Transaction::signal_generations`].
    #[doc(hidden)]
    pub async fn open_installing_signal(&mut self) -> Result<()> {
        self.require_system_full("Transaction::open_installing_signal")?;
        self.signal_read = false;
        if self.shared.signal.is_some() {
            self.install_signal_and_read().await?;
        }
        Ok(())
    }

    /// `sync_schema` with a signal: create the table, then read the generations (the transaction
    /// opened without the `BEGIN` read, since the table may not have existed) so the registry
    /// bump of the schema change it makes is credited to this process after the commit.
    pub(crate) async fn install_signal_and_read(&mut self) -> Result<()> {
        self.install_signal().await?;
        let full_refreshes = self
            .shared
            .signal
            .as_deref()
            .map_or(0, super::Signal::full_refreshes);
        let observed = self.signal_generations().await?;
        self.signal_state.began(observed, full_refreshes);
        Ok(())
    }

    /// The generations as seen by this transaction's snapshot (one statement).
    ///
    /// # Errors
    ///
    /// `Programming` when the table is missing, `Database` for other failures.
    pub async fn signal_generations(&mut self) -> Result<Generations> {
        let rows = self
            .query(
                "SELECT key, sum(weight)::int8 AS generation FROM erp_signal_generation \
                 GROUP BY key",
                &[],
            )
            .await?;
        let mut out = Generations::new();
        for row in rows {
            let key: String = row.try_get(0).map_err(undecodable)?;
            let generation: i64 = row.try_get(1).map_err(undecodable)?;
            let generation = u64::try_from(generation).map_err(|_| {
                Error::programming(format!(
                    "signal generation {generation} of {key:?} is negative"
                ))
            })?;
            out = out.with(SignalKey::parse(&key)?, generation);
        }
        Ok(out)
    }

    /// Record that `key` changed: insert one row (at most one per key per transaction) and, for
    /// [`SignalKey::Registry`], [`SignalKey::Policy`] and [`SignalKey::Membership`], send the wake-up
    /// ([`notify_once`](Self::notify_once) on the signal channel). Every process reads the new
    /// generation in its next transaction; listeners wake at the commit for registry, policy and
    /// membership changes and see named-cache changes at their next poll. Named-cache bumps send no
    /// notification: they are frequent, the `BEGIN` read of every transaction already carries them,
    /// and every notifying commit takes the cluster-wide notify lock (design note, measurement).
    ///
    /// With [`SelfApplied::Yes`] the caller states that this process already applied its own change
    /// in memory; after the commit the process does not refresh for this bump (a concurrent bump of
    /// another process is still seen).
    ///
    /// # Errors
    ///
    /// `Programming` inside an isolated scope (onchange), whose changes are always discarded;
    /// `Database` when the insert fails (for example the table is missing: run
    /// [`install_signal`](Self::install_signal)).
    pub async fn signal_bump(&mut self, key: &SignalKey, applied: SelfApplied) -> Result<()> {
        if self.in_isolated_scope() {
            return Err(Error::programming(
                "signal_bump inside an isolated scope (onchange): its changes are always \
                 discarded, so the signal would announce a change that never happens",
            ));
        }
        if let Some(i) = self.signal_state.bumped(key) {
            // one commit, one signal; only the stronger "self applied" claim survives
            if applied == SelfApplied::No
                && let What::Bump(_, claim) = &mut self.signal_state.sent[i].what
            {
                *claim = SelfApplied::No;
            }
            return Ok(());
        }
        let text = key.to_key_string();
        self.execute(BUMP_SQL, &[&text]).await?;
        let depth = self.savepoint_depth();
        self.signal_state.sent.push(Sent {
            depth,
            what: What::Bump(key.clone(), applied),
        });
        match key {
            SignalKey::Registry | SignalKey::Policy | SignalKey::Membership => {
                self.notify_once(&NotifyChannel::signal()).await?;
            }
            // read at BEGIN by every transaction and by the listener's poll: no wake-up needed
            SignalKey::Cache(_) => {}
        }
        Ok(())
    }

    /// Give the request role exactly what it needs on the signal table: `SELECT` (the `BEGIN`
    /// read) and `INSERT` of the `key` column only (bumps), after revoking everything else. The
    /// role can then neither choose a weight (each of its rows weighs 1, so `sum(weight)` cannot
    /// overflow and no row can lower a generation) nor delete or update rows; compaction and
    /// installation run as the owner. calls this when it sets up the roles. Granting
    /// twice gives the same privileges.
    ///
    /// Changing privileges is DDL: only a transaction of the `System` principal with full
    /// elevation may call this. The grant must restrict exactly the named role, so these are
    /// refused (the same checks as [`grant_node_role`](Self::grant_node_role)): `PUBLIC`, `NONE`
    /// and the predefined `pg_*` roles, a role that does not exist, the table owner (revoking from
    /// it would take away what installation and compaction need), a superuser, a member of the
    /// owner (directly or not, whether it inherits the owner's rights or may only `SET ROLE` to
    /// it), a role holding the `ADMIN OPTION` on the owner or on a role leading to it, or with
    /// `CREATEROLE` or `REPLICATION`, and a role that reaches, through any of its memberships, a
    /// role with one of those attributes, `pg_write_all_data`, `pg_read_all_data` or one of the
    /// server program and file roles (it keeps access to every table whatever is revoked). The
    /// checks hold when the grant runs: after changing the role, grant again.
    ///
    /// # Errors
    ///
    /// `Access` for any other principal; `Programming` for an invalid or refused role name and
    /// when the table is missing ([`install_signal`](Self::install_signal) first); `Database`
    /// when a statement fails (this role may not grant).
    pub async fn grant_signal_request_role(&mut self, role: &str) -> Result<()> {
        let role = self.check_grantee(&SIGNAL_GRANT, role).await?;
        self.batch(&format!(
            "REVOKE ALL ON erp_signal_generation FROM {role};\n\
             GRANT SELECT, INSERT (key) ON erp_signal_generation TO {role}"
        ))
        .await
    }

    /// Send a notification on `channel` when this transaction has not already: at most one
    /// statement per channel per transaction (a savepoint rollback takes back what it sent).
    /// The notification has an empty payload and there is no way to give it one: a notification
    /// only tells a listener to look at the rows. Returns whether a statement was sent.
    ///
    /// PostgreSQL delivers it at commit. Every transaction that notifies takes a cluster-wide lock
    /// at commit (`PreCommit_Notify`), so use it for low-rate events only.
    ///
    /// # Errors
    ///
    /// `Database` when the statement fails.
    pub async fn notify_once(&mut self, channel: &NotifyChannel) -> Result<bool> {
        if self.signal_state.notified(channel) {
            return Ok(false);
        }
        self.query(NOTIFY_SQL, &[&channel.as_str()]).await?;
        let depth = self.savepoint_depth();
        self.signal_state.sent.push(Sent {
            depth,
            what: What::Notify(channel.clone()),
        });
        Ok(true)
    }

    /// Bump `cache:<name>` for every named cache this transaction invalidated.
    /// This process already cleared them, so the bumps are [`SelfApplied::Yes`].
    pub(crate) async fn signal_cache_invalidations(&mut self) -> Result<()> {
        for name in self.engine.ormcache.cleared_names() {
            let key = SignalKey::cache(&name)?;
            self.signal_bump(&key, SelfApplied::Yes).await?;
        }
        Ok(())
    }

    /// Replace the rows of `key` older than `older_than` by one row with their summed weight, so
    /// the generation does not change and the per-transaction read stays small. Does nothing
    /// (returns 0) when fewer than two rows qualify. Needs `DELETE` on the table: run it from the
    /// maintenance role, not the request role. Returns the number of rows removed. The summary row
    /// carries the summed weight, so it fails with the table's `CHECK` (`23514`) once that sum
    /// exceeds [`MAX_SIGNAL_WEIGHT`](super::MAX_SIGNAL_WEIGHT) (about 10^12 bumps of one key).
    ///
    /// # Errors
    ///
    /// `Database` when the statement fails (a concurrent compaction under `REPEATABLE READ` gives
    /// a serialization failure that [`Orm::run`](crate::Orm::run) retries).
    pub async fn signal_compact(&mut self, key: &SignalKey, older_than: Duration) -> Result<u64> {
        let text = key.to_key_string();
        let secs = older_than.as_secs_f64();
        let rows = self.query(COMPACT_SQL, &[&text, &secs]).await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::programming("signal compaction returned no row"))?;
        let removed: i64 = row.try_get(0).map_err(undecodable)?;
        u64::try_from(removed)
            .map_err(|_| Error::programming("signal compaction removed a negative row count"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::MAX_SIGNAL_WEIGHT;
    use super::*;

    #[test]
    fn install_sql_bounds_the_weight() {
        assert!(INSTALL_SQL.contains(&format!("CHECK (weight BETWEEN 1 AND {MAX_SIGNAL_WEIGHT})")));
    }

    #[test]
    fn released_bumps_survive_a_sibling_rollback() {
        let mut state = TxSignal::default();
        state.began(Generations::new(), 0);
        state.sent.push(Sent {
            depth: 1,
            what: What::Bump(SignalKey::Registry, SelfApplied::Yes),
        });
        state.released_to(0);
        state.rolled_back_to(0);
        assert_eq!(state.bumped(&SignalKey::Registry), Some(0));
    }
}
