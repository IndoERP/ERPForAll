//! Cross-process change signals.
//!
//! Three things change under a running process without it knowing: the registry (DDL, installs),
//! the access policy and the named caches. Each has a *generation* kept in the insert-only table
//! `erp_signal_generation` (the generation of a key is the sum of the weights of its rows, so it
//! needs no sequence, no lock, and detects every bump whatever the commit order).
//!
//! - **Truth is a row.** With a [`Signal`] installed on the [`Orm`](crate::Orm), a transaction's
//!   first message is `BEGIN …; SELECT <generations>` (one round trip). A request that reads a
//!   newer generation than the process applied waits for one shared refresh and then proceeds, or
//!   fails; it never runs on stale state silently.
//! - **NOTIFY is a wake-up.** [`Transaction::notify_once`] sends a payload-free notification, at
//!   most once per channel per transaction. A [`ListenerHandle`] (one per process and database)
//!   turns it into an immediate read and apply, with a poll fallback and catch-up on reconnect.
//!
//! Notification payloads never carry data: [`Transaction::notify_once`] has no payload parameter.

pub(crate) mod bump_trigger;

/// Native composition attestation of the exact built-in signal function body.
/// This grants no addon capability: callers must also validate the trigger's structural catalog
/// attributes, declared ownership, signature and configuration. Search-path configuration must
/// use PostgreSQL's own `quote_ident` canonicalization. Uses the same source as schema DDL.
#[doc(hidden)]
pub fn builtin_signal_function_matches(
    signal: erp_registry::TriggerSignal,
    schema: &str,
    body: &str,
) -> bool {
    let sql = bump_trigger::create_function(signal, schema);
    let expected = sql
        .split_once("AS $erp$")
        .and_then(|(_, rest)| rest.split_once("$erp$;"))
        .map(|(source, _)| source);
    expected == Some(body)
}

pub(crate) mod follow_delete;
mod listener;
mod tx;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use erp_fields::{Condition, Error, ErrorKind, Result};
use tokio_postgres::SimpleQueryMessage;

use crate::dispatch::BoxFuture;
pub use listener::{ListenerHandle, ListenerOptions};
pub use tx::SelfApplied;
pub(crate) use tx::{INSTALL_SQL, TxSignal, upgrade_statements};

/// The channel the generation wake-up uses. `NOTIFY` is per database in PostgreSQL, so the name
/// carries no database.
pub const SIGNAL_CHANNEL: &str = "erp_signal";

/// The table holding the generations.
pub const SIGNAL_TABLE: &str = "erp_signal_generation";

/// SQLSTATE-shaped code reported with the error a transaction ends with when the refresh replaced
/// its [`Orm`](crate::Orm) ([`Refreshed::Superseded`]), for logs and clients. Not a PostgreSQL code,
/// and not what callers match on: a database function can raise the same code. Match with
/// [`Error::is_superseded`] (the error carries [`Condition::OrmSuperseded`]).
pub const SUPERSEDED_SQLSTATE: &str = "ERP01";

/// Largest weight one generation row may carry (the table's `CHECK`). Requests insert weight 1
/// (the request role may only insert the `key` column); only compaction writes larger weights, so
/// `sum(weight)` cannot overflow `int8` in practice (2^23 rows of the maximum weight would be
/// needed) and a row can never lower a generation.
pub const MAX_SIGNAL_WEIGHT: i64 = 1 << 40;

/// Default of [`Signal::with_apply_timeout`].
pub const DEFAULT_APPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// The statement that reads every generation (also the second half of the opening message).
pub(crate) const READ_SQL: &str =
    "SELECT key, sum(weight)::int8 FROM erp_signal_generation GROUP BY key";

const MAX_NAME_LEN: usize = 64;

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

/// The name of an ormcache: lowercase ASCII letters, digits, `_` and `.`, 1 to 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CacheName(String);

impl CacheName {
    /// Validate a cache name.
    ///
    /// # Errors
    ///
    /// A `Programming` error naming the rule when `name` is empty, longer than 64 bytes or has a
    /// character outside `[a-z0-9_.]`.
    pub fn new(name: &str) -> Result<Self> {
        if valid_name(name) {
            Ok(Self(name.to_owned()))
        } else {
            Err(Error::programming(format!(
                "invalid signal cache name {name:?}: 1 to {MAX_NAME_LEN} characters of [a-z0-9_.]"
            )))
        }
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CacheName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a generation counts: the registry, the access policy or one named cache.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SignalKey {
    /// Models, fields, schema: a change makes every process rebuild its registry.
    Registry,
    /// Record rules and access lists.
    Policy,
    /// Who belongs to what: group memberships, companies, `active` and `share` of users. Nothing
    /// is compiled from it, so applying a change is a no-op; the generation exists so a replica
    /// that has not replayed a membership change is detected by the routing lag guard.
    Membership,
    /// A named ormcache (`ormcache` in [`Transaction`](crate::Transaction)).
    Cache(CacheName),
}

impl SignalKey {
    /// The key of the cache `name`.
    ///
    /// # Errors
    ///
    /// See [`CacheName::new`].
    pub fn cache(name: &str) -> Result<Self> {
        CacheName::new(name).map(Self::Cache)
    }

    /// The text stored in the table: `registry`, `policy`, `membership` or `cache:<name>`.
    pub fn to_key_string(&self) -> String {
        match self {
            Self::Registry => "registry".to_owned(),
            Self::Policy => "policy".to_owned(),
            Self::Membership => "membership".to_owned(),
            Self::Cache(name) => format!("cache:{name}"),
        }
    }

    /// Parse the stored text.
    ///
    /// # Errors
    ///
    /// A `Programming` error for anything that is not `registry`, `policy`, `membership` or
    /// `cache:<name>`.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "registry" => Ok(Self::Registry),
            "policy" => Ok(Self::Policy),
            "membership" => Ok(Self::Membership),
            other => match other.strip_prefix("cache:") {
                Some(name) => Self::cache(name),
                None => Err(Error::programming(format!(
                    "unknown signal key {text:?}: expected registry, policy, membership or cache:<name>"
                ))),
            },
        }
    }
}

impl fmt::Display for SignalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_key_string())
    }
}

/// A named `NOTIFY` channel: a lowercase identifier (`[a-z][a-z0-9_]{0,62}`). Notifications sent
/// through [`Transaction::notify_once`] carry no payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NotifyChannel(String);

impl NotifyChannel {
    /// Validate a channel name.
    ///
    /// # Errors
    ///
    /// A `Programming` error when the name is not a lowercase identifier of at most 63 bytes.
    pub fn new(name: &str) -> Result<Self> {
        let mut bytes = name.bytes();
        let ok = name.len() <= 63
            && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if ok {
            Ok(Self(name.to_owned()))
        } else {
            Err(Error::programming(format!(
                "invalid notify channel {name:?}: [a-z][a-z0-9_]{{0,62}}"
            )))
        }
    }

    /// The signal channel ([`SIGNAL_CHANNEL`]).
    pub fn signal() -> Self {
        Self(SIGNAL_CHANNEL.to_owned())
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The generation of every key at one moment. A key with no rows has generation 0.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Generations(BTreeMap<SignalKey, u64>);

impl Generations {
    /// No key has a row.
    pub fn new() -> Self {
        Self::default()
    }

    /// The generation of `key` (0 when it has no rows).
    pub fn get(&self, key: &SignalKey) -> u64 {
        self.0.get(key).copied().unwrap_or(0)
    }

    /// Set the generation of `key` (tests and baselines).
    #[must_use]
    pub fn with(mut self, key: SignalKey, generation: u64) -> Self {
        self.0.insert(key, generation);
        self
    }

    /// The keys that have rows, with their generations.
    pub fn iter(&self) -> impl Iterator<Item = (&SignalKey, u64)> {
        self.0.iter().map(|(k, v)| (k, *v))
    }

    /// Parse the rows of [`READ_SQL`] run as a prepared statement.
    pub(crate) fn from_rows(rows: &[tokio_postgres::Row]) -> Result<Self> {
        let mut out = BTreeMap::new();
        for row in rows {
            let key: String = row.try_get(0).map_err(undecodable)?;
            let sum: i64 = row.try_get(1).map_err(undecodable)?;
            let generation = u64::try_from(sum).map_err(|_| {
                Error::programming(format!("signal generation {sum} of {key:?} is negative"))
            })?;
            out.insert(SignalKey::parse(&key)?, generation);
        }
        Ok(Self(out))
    }

    /// Parse the result of [`READ_SQL`] run through the simple-query protocol.
    pub(crate) fn from_messages(messages: &[SimpleQueryMessage]) -> Result<Self> {
        let mut out = BTreeMap::new();
        for message in messages {
            let SimpleQueryMessage::Row(row) = message else {
                continue;
            };
            let key = row
                .get(0)
                .ok_or_else(|| Error::programming("signal read returned a NULL key"))?;
            let sum = row
                .get(1)
                .ok_or_else(|| Error::programming("signal read returned a NULL generation"))?;
            let generation: u64 = sum.parse().map_err(|_| {
                Error::programming(format!(
                    "signal generation {sum:?} of {key:?} is not a count"
                ))
            })?;
            out.insert(SignalKey::parse(key)?, generation);
        }
        Ok(Self(out))
    }
}

/// A generation read row that does not decode as `(text, int8)`.
pub(crate) fn undecodable(e: tokio_postgres::Error) -> Error {
    Error::new(
        ErrorKind::Database,
        format!("could not decode a signal generation row: {e}"),
    )
    .with_source(e)
}

/// What a refresh has to catch up with.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Changes {
    /// The registry generation moved.
    pub registry: bool,
    /// The policy generation moved.
    pub policy: bool,
    /// The membership generation moved (nothing to rebuild: memberships are read at every
    /// transaction start).
    pub membership: bool,
    /// The named caches whose generation moved (already cleared when the hook runs).
    pub caches: Vec<CacheName>,
    /// The generations this refresh brings the process to.
    pub observed: Generations,
    /// The generations went backwards (the database was restored or signal rows were deleted):
    /// rebuild everything. `registry` and `policy` are then both set and every named cache was
    /// cleared.
    pub regression: bool,
}

impl Changes {
    fn none() -> Self {
        Self {
            registry: false,
            policy: false,
            membership: false,
            caches: Vec::new(),
            observed: Generations::new(),
            regression: false,
        }
    }

    fn is_empty(&self) -> bool {
        !self.registry && !self.policy && !self.membership && self.caches.is_empty()
    }
}

/// A key whose observed generation is lower than the applied one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lower {
    key: SignalKey,
    observed: u64,
    applied: u64,
}

/// [`Signal::diff`]: what is newer, what is lower, and the apply epoch the comparison was made at.
#[derive(Debug)]
pub(crate) struct Diff {
    changes: Changes,
    lower: Vec<Lower>,
    epoch: u64,
}

impl Diff {
    /// A lower generation is a regression only when the read that produced it started after the
    /// applied generations were last changed (`read_epoch` is the apply epoch loaded before the
    /// read was sent): every applied value then came from an older snapshot, and a generation only
    /// grows. Otherwise the read may simply have raced a newer apply.
    fn is_regression(&self, read_epoch: u64) -> bool {
        !self.lower.is_empty() && read_epoch == self.epoch
    }

    fn describe_lower(&self) -> String {
        self.lower
            .iter()
            .map(|l| format!("{} {} -> {}", l.key, l.applied, l.observed))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// How a refresh ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refreshed {
    /// The state was updated in place; the transaction that triggered the refresh continues.
    InPlace,
    /// A new [`Orm`](crate::Orm) replaced this one (a new registry). Transactions of this `Orm`
    /// end with [`SUPERSEDED_SQLSTATE`]; the server retries them on the new one.
    /// Returned from a full refresh after a regression ([`Changes::regression`]), the request that
    /// triggered it still fails with [`Condition::SignalRegression`]; its retry on this `Orm` then
    /// fails with [`Condition::OrmSuperseded`] and the server retries it on the new one.
    Superseded,
}

/// The hook the server implements: rebuild the registry or the policy snapshot after a change.
///
/// Called with the apply lock held, so concurrent requests cause one refresh. The requests that wait
/// for it hold pooled connections of the [`Orm`](crate::Orm) (their transaction has begun), so the
/// hook **must use its own connection** (or a separate pool), never one from this `Orm`'s pool: with
/// every pooled connection held by a waiter it would wait for itself. Waiting and running are both
/// bounded by [`Signal::with_apply_timeout`]; a hook that overruns is cancelled (dropped), so it must
/// be cancel-safe. An error fails the request that triggered it and leaves the registry and policy
/// generations unapplied, so the next request tries again (the changed named caches were already
/// cleared and stay applied). When [`Changes::regression`] is set, rebuild
/// everything.
pub trait Refresh: Send + Sync + 'static {
    /// Bring the process up to `changes.observed`.
    fn refresh<'a>(&'a self, changes: &'a Changes) -> BoxFuture<'a, Result<Refreshed>>;
}

#[derive(Debug, Default)]
struct Counters {
    listener_up: AtomicBool,
    /// A listener was started and not stopped since ([`Signal::listener_down`]).
    listener_expected: AtomicBool,
    reconnects: AtomicU64,
    notifications: AtomicU64,
    polls: AtomicU64,
    applies: AtomicU64,
    refreshes: AtomicU64,
    regressions: AtomicU64,
}

/// A snapshot of the signal metrics (`signal_listener_up` and friends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignalStats {
    /// The listener connection is up (`signal_listener_up`).
    pub listener_up: bool,
    /// Connection attempts after the first.
    pub reconnects: u64,
    /// Wake-up notifications received.
    pub notifications: u64,
    /// Fallback reads by the listener.
    pub polls: u64,
    /// Times new generations were applied (once per change set, not per transaction).
    pub applies: u64,
    /// Times the [`Refresh`] hook ran.
    pub refreshes: u64,
    /// Times a read found the generations lower than applied (`signal_regressions`): a restored
    /// database or deleted signal rows. Each one fails its request and forces a full refresh.
    pub regressions: u64,
}

/// The signal state of one database in one process: the generations applied so far and the
/// refresh hook. Install it with [`OrmBuilder::signal`](crate::OrmBuilder::signal).
pub struct Signal {
    applied: RwLock<Generations>,
    /// Moves (under the `applied` write lock) every time `applied` changes.
    apply_epoch: AtomicU64,
    /// Full refreshes started (after a regression). Moved before every named cache is cleared: a
    /// transaction that loaded an older value never publishes ormcache values.
    full_refreshes: AtomicU64,
    /// Held for writing by a refresh (exclusive), for reading by a routed transaction pinning its
    /// policy: pins run side by side and only collide with a refresh.
    apply_lock: tokio::sync::RwLock<()>,
    apply_timeout: Duration,
    refresh: Option<Arc<dyn Refresh>>,
    superseded: AtomicBool,
    counters: Counters,
    /// Channels other subsystems (jobs, outbox consumers, the bus) listen on through this
    /// signal's listener, each with the wake-up it gets.
    subscribers: Mutex<Vec<(NotifyChannel, Arc<tokio::sync::Notify>)>>,
    /// Moves when a subscription is added: a running listener `LISTEN`s on the new channel.
    subscription_epoch: AtomicU64,
}

/// Most channel subscriptions one [`Signal`] takes: the listener's `LISTEN` set is bounded by
/// the channels the binary registers, not by data.
pub const MAX_SUBSCRIPTIONS: usize = 64;

impl fmt::Debug for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signal")
            .field("applied", &self.applied())
            .field("superseded", &self.is_superseded())
            .finish_non_exhaustive()
    }
}

impl Signal {
    /// A signal state that has applied `baseline`. Read the baseline *before* building the
    /// registry (see [`Signal::read`]): a change that lands while the registry is built is then
    /// seen as newer than the baseline.
    pub fn new(baseline: Generations) -> Self {
        Self {
            applied: RwLock::new(baseline),
            apply_epoch: AtomicU64::new(0),
            full_refreshes: AtomicU64::new(0),
            apply_lock: tokio::sync::RwLock::new(()),
            apply_timeout: DEFAULT_APPLY_TIMEOUT,
            refresh: None,
            superseded: AtomicBool::new(false),
            counters: Counters::default(),
            subscribers: Mutex::new(Vec::new()),
            subscription_epoch: AtomicU64::new(0),
        }
    }

    /// Ask the listener to wake the returned [`Notify`](tokio::sync::Notify) when a notification
    /// arrives on `channel` (payloads are never read), and once after every reconnect's catch-up,
    /// since notifications sent while the connection was down are lost. The listener sends
    /// `LISTEN` for the channel when it starts a session, or at its next wake-up when it is
    /// already running. One call per subscriber; call it before
    /// [`spawn_signal_listener`](crate::Orm::spawn_signal_listener) when you can.
    ///
    /// # Errors
    ///
    /// `Programming` for the signal channel itself (the listener owns it) and when
    /// [`MAX_SUBSCRIPTIONS`] subscriptions exist.
    pub fn subscribe(&self, channel: &NotifyChannel) -> Result<Arc<tokio::sync::Notify>> {
        if channel.as_str() == SIGNAL_CHANNEL {
            return Err(Error::programming(format!(
                "the channel {SIGNAL_CHANNEL} is the signal listener's own; subscribe to another"
            )));
        }
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if subscribers.len() >= MAX_SUBSCRIPTIONS {
            return Err(Error::programming(format!(
                "a Signal takes at most {MAX_SUBSCRIPTIONS} channel subscriptions"
            )));
        }
        let notify = Arc::new(tokio::sync::Notify::new());
        subscribers.push((channel.clone(), Arc::clone(&notify)));
        self.subscription_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(notify)
    }

    /// The distinct channels with a subscriber, in subscription order.
    pub(crate) fn subscribed_channels(&self) -> Vec<String> {
        let subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut seen: Vec<String> = Vec::new();
        for (channel, _) in subscribers.iter() {
            if !seen.iter().any(|c| c == channel.as_str()) {
                seen.push(channel.as_str().to_owned());
            }
        }
        seen
    }

    pub(crate) fn subscription_epoch(&self) -> u64 {
        self.subscription_epoch.load(Ordering::Acquire)
    }

    /// A notification arrived on `channel`: wake its subscribers.
    pub(crate) fn deliver(&self, channel: &str) {
        let subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (subscribed, notify) in subscribers.iter() {
            if subscribed.as_str() == channel {
                notify.notify_one();
            }
        }
    }

    /// Wake every subscriber once (after a reconnect's catch-up, or a new `LISTEN`).
    pub(crate) fn wake_subscribers(&self) {
        let subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, notify) in subscribers.iter() {
            notify.notify_one();
        }
    }

    /// Install the refresh hook. Without one, a registry or policy change makes transactions fail
    /// with an error saying so (a stale registry is never served silently).
    #[must_use]
    pub fn with_refresh(mut self, refresh: impl Refresh) -> Self {
        self.refresh = Some(Arc::new(refresh));
        self
    }

    /// Bound how long a request waits for the refresh another request is running, and how long the
    /// [`Refresh`] hook may run (default [`DEFAULT_APPLY_TIMEOUT`], 30 s). A hook that does not
    /// finish in time is cancelled (dropped) and the registry and policy generations stay
    /// unapplied, so the next request tries again; the hook must therefore be cancel-safe.
    ///
    /// # Errors
    ///
    /// `Programming` when `timeout` is zero (zero never means unlimited).
    pub fn with_apply_timeout(mut self, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() {
            return Err(Error::programming(
                "Signal::with_apply_timeout: the timeout must be non-zero (zero never means \
                 unlimited)",
            ));
        }
        self.apply_timeout = timeout;
        Ok(self)
    }

    /// Read the generations through `client`.
    ///
    /// # Errors
    ///
    /// `Database` when the read fails (the error names [`Transaction::install_signal`] when the
    /// table is missing).
    pub async fn read(client: &tokio_postgres::Client) -> Result<Generations> {
        let messages = client.simple_query(READ_SQL).await.map_err(read_error)?;
        Generations::from_messages(&messages)
    }

    /// The generations applied so far.
    pub fn applied(&self) -> Generations {
        self.applied
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// True once a refresh returned [`Refreshed::Superseded`].
    pub fn is_superseded(&self) -> bool {
        self.superseded.load(Ordering::Acquire)
    }

    /// The metrics.
    pub fn stats(&self) -> SignalStats {
        let c = &self.counters;
        SignalStats {
            listener_up: c.listener_up.load(Ordering::Relaxed),
            reconnects: c.reconnects.load(Ordering::Relaxed),
            notifications: c.notifications.load(Ordering::Relaxed),
            polls: c.polls.load(Ordering::Relaxed),
            applies: c.applies.load(Ordering::Relaxed),
            refreshes: c.refreshes.load(Ordering::Relaxed),
            regressions: c.regressions.load(Ordering::Relaxed),
        }
    }

    /// The apply epoch: load it before sending a generation read and pass it with the result, so a
    /// lower observation can be told apart from a read that raced a newer apply.
    pub(crate) fn apply_epoch(&self) -> u64 {
        self.apply_epoch.load(Ordering::Acquire)
    }

    /// The number of full refreshes started: load it before sending the read a transaction's
    /// snapshot begins with, and refuse that transaction's ormcache values when it has moved by
    /// commit (the regression refresh cleared every named cache, and generations that went
    /// backwards cannot tell an old snapshot from a new one).
    pub(crate) fn full_refreshes(&self) -> u64 {
        self.full_refreshes.load(Ordering::Acquire)
    }

    /// The applied generation of one key (no copy of the map).
    pub(crate) fn applied_generation(&self, key: &SignalKey) -> u64 {
        self.applied
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
    }

    /// Compare `observed` with what the process applied: the keys that are newer (the changes) and
    /// the keys that are lower, at the current apply epoch.
    fn diff(&self, observed: &Generations) -> Diff {
        let applied = self.applied.read().unwrap_or_else(PoisonError::into_inner);
        let epoch = self.apply_epoch.load(Ordering::Acquire);
        let mut changes = Changes::none();
        for (key, generation) in observed.iter() {
            if generation <= applied.get(key) {
                continue;
            }
            match key {
                SignalKey::Registry => changes.registry = true,
                SignalKey::Policy => changes.policy = true,
                SignalKey::Membership => changes.membership = true,
                SignalKey::Cache(name) => changes.caches.push(name.clone()),
            }
            changes.observed.0.insert(key.clone(), generation);
        }
        // a key with rows applied but none observed reads as 0: iterate the applied side
        let mut lower = Vec::new();
        for (key, applied_generation) in applied.iter() {
            let seen = observed.get(key);
            if seen < applied_generation {
                lower.push(Lower {
                    key: key.clone(),
                    observed: seen,
                    applied: applied_generation,
                });
            }
        }
        Diff {
            changes,
            lower,
            epoch,
        }
    }

    /// The keys whose `observed` generation (read on a replica) is lower than the one this process
    /// applied, described for the routing metric; `None` when none is lower. The replica's snapshot
    /// predates a change this process already runs on.
    pub(crate) fn behind(&self, observed: &Generations) -> Option<String> {
        let diff = self.diff(observed);
        (!diff.lower.is_empty()).then(|| diff.describe_lower())
    }

    /// Record the generations of `observed` whose key `which` selects (never lowering one), and
    /// move the apply epoch when one was selected.
    fn record_applied(&self, observed: &Generations, which: impl Fn(&SignalKey) -> bool) {
        let mut applied = self.applied.write().unwrap_or_else(PoisonError::into_inner);
        let mut selected = false;
        for (key, generation) in observed.iter().filter(|(key, _)| which(key)) {
            selected = true;
            if generation > applied.get(key) {
                applied.0.insert(key.clone(), generation);
            }
        }
        if selected {
            self.apply_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// After a full refresh: the process now runs on exactly `observed` (lower values included).
    fn replace_applied(&self, observed: &Generations) {
        let mut applied = self.applied.write().unwrap_or_else(PoisonError::into_inner);
        *applied = observed.clone();
        self.apply_epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// After a commit that bumped `key` and applied the change in this process itself: advance
    /// the applied generation by the transaction's own bump, but only when nothing else moved it
    /// since the transaction began (otherwise a refresh may run once more, never less).
    pub(crate) fn note_own_bump(&self, key: &SignalKey, began_at: u64) {
        let mut applied = self.applied.write().unwrap_or_else(PoisonError::into_inner);
        if applied.get(key) == began_at {
            applied.0.insert(key.clone(), began_at.saturating_add(1));
            self.apply_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Run the refresh hook, bounded by the apply timeout.
    async fn run_refresh(
        &self,
        refresh: &Arc<dyn Refresh>,
        changes: &Changes,
    ) -> Result<Refreshed> {
        self.counters.refreshes.fetch_add(1, Ordering::Relaxed);
        match tokio::time::timeout(self.apply_timeout, refresh.refresh(changes)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(Error::new(
                ErrorKind::Database,
                format!(
                    "the signal refresh hook did not finish within {:?} \
                     (Signal::with_apply_timeout): it was cancelled and the registry and policy \
                     generations stay unapplied, so the next request tries again",
                    self.apply_timeout
                ),
            )),
        }
    }

    pub(crate) fn set_listener_expected(&self, expected: bool) {
        self.counters
            .listener_expected
            .store(expected, Ordering::Release);
    }

    /// A listener was started ([`crate::Orm::spawn_signal_listener`]) and is not up: it is
    /// reconnecting or catching up, so this process may not know of the changes it would have
    /// applied. Without a listener ever started this is false (the process then learns of changes
    /// from the `BEGIN` read of its primary transactions only).
    pub(crate) fn listener_down(&self) -> bool {
        self.counters.listener_expected.load(Ordering::Acquire)
            && !self.counters.listener_up.load(Ordering::Acquire)
    }

    /// Run `pin` (take the policy snapshot a routed transaction decides with) at a moment when
    /// no refresh is running and the process has applied nothing newer than `observed`, the
    /// generations the replica's snapshot has. The apply lock is held for reading during `pin`
    /// (a refresh holds it for writing): a refresh swaps the policy before it records the
    /// generation, so without the lock the pinned policy could be newer than the replica's
    /// snapshot. Concurrent pins share the lock and never collide. `Err` says why it could not be
    /// pinned; the caller reroutes to the primary (design note §5).
    pub(crate) fn pin_not_ahead<R>(
        &self,
        observed: &Generations,
        pin: impl FnOnce() -> R,
    ) -> std::result::Result<R, PinRefused> {
        let Ok(_guard) = self.apply_lock.try_read() else {
            return Err(PinRefused::RefreshRunning);
        };
        match self.behind(observed) {
            Some(keys) => Err(PinRefused::Behind(keys)),
            None => Ok(pin()),
        }
    }

    /// A listener was started and is up. Without one this process learns of changes only from
    /// the `BEGIN` read of its own primary transactions.
    pub(crate) fn listener_running(&self) -> bool {
        self.counters.listener_expected.load(Ordering::Acquire)
            && self.counters.listener_up.load(Ordering::Acquire)
    }

    pub(crate) fn set_listener_up(&self, up: bool) {
        self.counters.listener_up.store(up, Ordering::Relaxed);
    }

    pub(crate) fn count_reconnect(&self) {
        self.counters.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_notification(&self) {
        self.counters.notifications.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_poll(&self) {
        self.counters.polls.fetch_add(1, Ordering::Relaxed);
    }
}

/// Why [`Signal::pin_not_ahead`] refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PinRefused {
    /// A refresh holds (or waits for) the apply lock.
    RefreshRunning,
    /// The process applied generations newer than the replica's snapshot (the keys).
    Behind(String),
}

/// What applying a set of generations did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Applied {
    UpToDate,
    InPlace,
    Superseded,
}

impl crate::transaction::Shared {
    /// Bring the process to `observed`: nothing when it is not newer than what was applied; else,
    /// under the apply lock (so concurrent callers share one refresh), bump the schema epoch
    /// (registry), record the changed named caches' generations and then clear those caches, run
    /// the [`Refresh`] hook (registry, policy) and record the registry and policy generations once
    /// it succeeded. Recording a cache generation before clearing the cache closes the window in
    /// which a transaction whose snapshot predates the change first uses the cleared cache: its
    /// values read as stale at commit and are not published. When the hook fails, the caches stay
    /// applied (they were cleared) and the registry and policy stay unapplied, so the next request
    /// runs the hook again.
    ///
    /// `read_epoch` is [`Signal::apply_epoch`] loaded before the read that produced `observed` was
    /// sent. A key lower than applied in a read that started after the last
    /// apply is a regression: it is counted, logged at error, forces a full refresh and fails the
    /// caller with [`Condition::SignalRegression`]. A lower key in a read that raced a newer apply
    /// changes nothing.
    pub(crate) async fn apply_signal(
        &self,
        observed: &Generations,
        read_epoch: u64,
    ) -> Result<Applied> {
        let Some(signal) = &self.signal else {
            return Ok(Applied::UpToDate);
        };
        if signal.is_superseded() {
            return Err(superseded_error());
        }
        let diff = signal.diff(observed);
        if diff.changes.is_empty() && !diff.is_regression(read_epoch) {
            return Ok(Applied::UpToDate);
        }
        let Ok(_guard) =
            tokio::time::timeout(signal.apply_timeout, signal.apply_lock.write()).await
        else {
            return Err(Error::new(
                ErrorKind::Lock,
                format!(
                    "waited {:?} for the signal refresh another request is running \
                     (Signal::with_apply_timeout) and gave up; a refresh hook must not wait for \
                     a pooled connection of this Orm, it uses its own connection",
                    signal.apply_timeout
                ),
            ));
        };
        if signal.is_superseded() {
            return Err(superseded_error());
        }
        let diff = signal.diff(observed);
        if diff.is_regression(read_epoch) {
            return Err(self.recover_from_regression(signal, observed, &diff).await);
        }
        let changes = diff.changes;
        if changes.is_empty() {
            return Ok(Applied::UpToDate);
        }
        if changes.registry {
            self.bump_schema_epoch();
        }
        if !changes.caches.is_empty() {
            // applied first, cleared second: a transaction that began before the change and first
            // uses one of these caches after the clear then holds values older than the applied
            // generation, which `publish_ormcache` refuses
            signal.record_applied(&changes.observed, is_cache);
            for name in &changes.caches {
                self.ormcache.clear(name.as_str());
            }
        }
        let mut outcome = Refreshed::InPlace;
        if changes.registry || changes.policy {
            let Some(refresh) = &signal.refresh else {
                return Err(Error::new(
                    ErrorKind::Programming,
                    format!(
                        "the {} generation moved but no signal refresh hook is installed \
                         (Signal::with_refresh); refusing to serve on stale state",
                        if changes.registry {
                            "registry"
                        } else {
                            "policy"
                        }
                    ),
                ));
            };
            outcome = signal.run_refresh(refresh, &changes).await?;
        }
        signal.record_applied(&changes.observed, |key| !is_cache(key));
        signal.counters.applies.fetch_add(1, Ordering::Relaxed);
        if outcome == Refreshed::Superseded {
            signal.superseded.store(true, Ordering::Release);
            return Ok(Applied::Superseded);
        }
        Ok(Applied::InPlace)
    }

    /// The generations went backwards (called under the apply lock): count it, log it, rebuild
    /// everything and run on the observed generations from now on. Returns the error the caller
    /// fails with, whatever the refresh did: a restore under a running process is never silent.
    /// Without a refresh hook nothing can be rebuilt, the applied generations stay, and every
    /// request fails the same way until the process restarts.
    ///
    /// The full-refresh count moves before every named cache is cleared, so no transaction that
    /// began before this refresh publishes ormcache values (generations that went backwards cannot
    /// tell its old snapshot apart). The generations are recorded only once the hook succeeded.
    ///
    /// When the hook returns [`Refreshed::Superseded`] the caller still fails with
    /// [`Condition::SignalRegression`]; its retry on the same `Orm` then fails with
    /// [`Condition::OrmSuperseded`] and goes to the new `Orm`.
    async fn recover_from_regression(
        &self,
        signal: &Signal,
        observed: &Generations,
        diff: &Diff,
    ) -> Error {
        signal.counters.regressions.fetch_add(1, Ordering::Relaxed);
        let detail = diff.describe_lower();
        tracing::error!(
            target: "erp::signal",
            lower = %detail,
            "signal generations went backwards (database restored or signal rows deleted); \
             forcing a full refresh"
        );
        let regression = |what: &str| {
            Error::new(
                ErrorKind::Database,
                format!(
                    "the signal generations went backwards ({detail}): the database was restored \
                     or signal rows were deleted; {what}"
                ),
            )
            .with_condition(Condition::SignalRegression)
        };
        let Some(refresh) = &signal.refresh else {
            return regression(
                "no refresh hook is installed (Signal::with_refresh), so this process refuses to \
                 serve until it restarts",
            );
        };
        signal.full_refreshes.fetch_add(1, Ordering::AcqRel);
        self.bump_schema_epoch();
        self.ormcache.clear_all();
        let mut caches: Vec<CacheName> = signal
            .applied()
            .iter()
            .chain(observed.iter())
            .filter_map(|(key, _)| match key {
                SignalKey::Cache(name) => Some(name.clone()),
                // registry and policy are covered by the flags below
                SignalKey::Registry | SignalKey::Policy | SignalKey::Membership => None,
            })
            .collect();
        caches.sort();
        caches.dedup();
        let changes = Changes {
            registry: true,
            policy: true,
            membership: true,
            caches,
            observed: observed.clone(),
            regression: true,
        };
        match signal.run_refresh(refresh, &changes).await {
            Ok(outcome) => {
                signal.replace_applied(observed);
                signal.counters.applies.fetch_add(1, Ordering::Relaxed);
                if outcome == Refreshed::Superseded {
                    signal.superseded.store(true, Ordering::Release);
                }
                regression("this process ran a full refresh; retry the request")
            }
            Err(error) => regression("the full refresh failed and the next request tries again")
                .with_source(error),
        }
    }
}

fn is_cache(key: &SignalKey) -> bool {
    matches!(key, SignalKey::Cache(_))
}

pub(crate) fn superseded_error() -> Error {
    Error::new(
        ErrorKind::Concurrency,
        "the registry changed in another process and was reloaded: this Orm is superseded; retry \
         on the current one",
    )
    .with_sqlstate(Some(SUPERSEDED_SQLSTATE.to_owned()))
    .with_condition(Condition::OrmSuperseded)
}

/// Map a failed generation read; a missing table gets the message that tells how to fix it.
pub(crate) fn read_error(e: tokio_postgres::Error) -> Error {
    if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) {
        return Error::new(
            ErrorKind::Programming,
            format!(
                "the signal table {SIGNAL_TABLE} is missing: run Transaction::install_signal \
                 (Orm::sync_schema does it when the Orm has a Signal) before serving"
            ),
        )
        .with_source(e)
        .with_sqlstate(Some("42P01".to_owned()));
    }
    Error::new(
        ErrorKind::Database,
        format!("could not read the signal generations: {e}"),
    )
    .with_source(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_round_trip_and_rejections() {
        for key in [
            SignalKey::Registry,
            SignalKey::Policy,
            SignalKey::Membership,
            SignalKey::cache("res.partner").unwrap_or(SignalKey::Registry),
        ] {
            assert_eq!(SignalKey::parse(&key.to_key_string()).unwrap(), key);
        }
        for bad in [
            "",
            "Registry",
            "cache:",
            "cache:UP",
            "cache:é",
            "cache:a b",
            "x",
        ] {
            assert_eq!(
                SignalKey::parse(bad).unwrap_err().kind(),
                ErrorKind::Programming,
                "{bad:?}"
            );
        }
        let long = "a".repeat(65);
        let err = CacheName::new(&long).unwrap_err();
        assert_eq!(
            err.message(),
            format!("invalid signal cache name {long:?}: 1 to 64 characters of [a-z0-9_.]")
        );
        assert!(CacheName::new(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn channel_validation() {
        assert!(NotifyChannel::new("erp_signal").is_ok());
        for bad in ["", "1a", "A", "a-b", "a b", &"a".repeat(64)] {
            let err = NotifyChannel::new(bad).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Programming, "{bad:?}");
            assert_eq!(
                err.message(),
                format!("invalid notify channel {bad:?}: [a-z][a-z0-9_]{{0,62}}")
            );
        }
    }

    /// Ruling (c): a lower generation read after the last apply is a regression; one read before
    /// a newer apply is a race; a key whose rows are all gone reads as 0.
    #[test]
    fn diff_tells_regressions_from_races() {
        let reg = SignalKey::Registry;
        let signal = Signal::new(Generations::new().with(reg.clone(), 5));
        let read_epoch = signal.apply_epoch();
        let equal = signal.diff(&Generations::new().with(reg.clone(), 5));
        assert!(equal.changes.is_empty() && equal.lower.is_empty());
        assert!(!equal.is_regression(read_epoch));

        let lower = signal.diff(&Generations::new().with(reg.clone(), 4));
        assert!(lower.changes.is_empty());
        assert!(lower.is_regression(read_epoch));
        assert_eq!(lower.describe_lower(), "registry 5 -> 4");

        let gone = signal.diff(&Generations::new());
        assert!(gone.is_regression(read_epoch));
        assert_eq!(gone.describe_lower(), "registry 5 -> 0");

        // a read sent before the apply below raced it: not a regression
        signal.record_applied(
            &signal
                .diff(&Generations::new().with(reg.clone(), 6))
                .changes
                .observed,
            |_| true,
        );
        let raced = signal.diff(&Generations::new().with(reg.clone(), 5));
        assert!(!raced.lower.is_empty());
        assert!(!raced.is_regression(read_epoch));
        assert!(raced.is_regression(signal.apply_epoch()));

        let newer = signal.diff(&Generations::new().with(reg, 7)).changes;
        assert!(newer.registry && !newer.policy && newer.caches.is_empty());
    }

    #[test]
    fn apply_timeout_must_be_non_zero() {
        let err = Signal::new(Generations::new())
            .with_apply_timeout(Duration::ZERO)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "Signal::with_apply_timeout: the timeout must be non-zero (zero never means unlimited)"
        );
    }

    /// a routed transaction pins the policy only when no refresh is running and the
    /// process has applied nothing newer than the replica's snapshot.
    #[test]
    fn the_policy_pin_is_refused_when_it_would_be_newer_than_the_snapshot() {
        let policy = SignalKey::Policy;
        let signal = Signal::new(Generations::new().with(policy.clone(), 2));
        let snapshot = |generation| Generations::new().with(SignalKey::Policy, generation);
        assert_eq!(signal.pin_not_ahead(&snapshot(2), || 7), Ok(7));
        assert_eq!(
            signal.pin_not_ahead(&snapshot(3), || 7),
            Ok(7),
            "replica ahead"
        );
        assert_eq!(
            signal.pin_not_ahead(&snapshot(1), || 7),
            Err(PinRefused::Behind("policy 2 -> 1".to_owned())),
            "the process is ahead of the snapshot"
        );
        // a refresh is running: the policy may already be swapped, the generation not recorded
        let running = signal.apply_lock.try_write().unwrap();
        assert_eq!(
            signal.pin_not_ahead(&snapshot(2), || 7),
            Err(PinRefused::RefreshRunning)
        );
        drop(running);
        assert_eq!(signal.pin_not_ahead(&snapshot(2), || 7), Ok(7));
        // pins share the lock: one in progress does not refuse another
        let reading = signal.apply_lock.try_read().unwrap();
        assert_eq!(signal.pin_not_ahead(&snapshot(2), || 7), Ok(7));
        drop(reading);
    }

    #[test]
    fn a_listener_that_is_expected_and_not_up_is_down() {
        let signal = Signal::new(Generations::new());
        assert!(!signal.listener_down(), "no listener was started");
        signal.set_listener_expected(true);
        assert!(signal.listener_down(), "started, not up yet");
        signal.set_listener_up(true);
        assert!(!signal.listener_down());
        signal.set_listener_up(false);
        assert!(signal.listener_down(), "reconnecting");
        signal.set_listener_expected(false);
        assert!(!signal.listener_down(), "stopped on purpose");
    }

    #[test]
    fn superseded_error_carries_the_condition() {
        let err = superseded_error();
        assert!(err.is_superseded());
        assert_eq!(err.kind(), ErrorKind::Concurrency);
        assert_eq!(err.sqlstate(), Some(SUPERSEDED_SQLSTATE));
    }
}
