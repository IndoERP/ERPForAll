//! A node's membership in one database: the public handle (design §3, §6.4, §14).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use deadpool_postgres::Pool;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::clock::{self, Tick};
use super::heartbeat::{self, Command};
use super::identity::MembershipClaim;
use super::{NodeError, NodeIdentity, NodeView, Readiness, UnreadyReason};

/// Test seam: a fault injected into one registration ([`NodeMembership::register_with_fault_for_tests`]).
#[cfg(feature = "test-faults")]
#[doc(hidden)]
#[non_exhaustive]
pub enum RegisterFault {
    /// Wait for the sender between the two flights, outside the client deadline, while the
    /// transaction holds the readiness lock: a stopped process (SIGSTOP, a paused VM).
    PauseBetweenFlights(oneshot::Receiver<()>),
    /// The registration commits, then its answer is treated as lost (the client deadline passed).
    LoseCommitReply,
}

/// Without `test-faults` no fault exists.
#[cfg(not(feature = "test-faults"))]
pub(super) enum RegisterFault {}

/// What the fence seam reports ([`NodeMembership::fence_timing_for_tests`]).
#[cfg(feature = "test-faults")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct FenceTiming {
    /// Time since the last successful renewal (or the registration) was sent.
    pub since_sent: Duration,
    /// Time left until the self-fence deadline (`None`: passed).
    pub until_fence: Option<Duration>,
}

/// State shared by the handles and the heartbeat task. `is_serving` reads three atomics and the
/// membership clock: no lock, no allocation.
pub(super) struct Shared {
    pub(super) database: Arc<str>,
    pub(super) identity: Arc<NodeIdentity>,
    /// The owner asked to drain or shut down: sticky, so work the task already started (a
    /// transition that turns the node ready) can never admit requests again.
    leaving: AtomicBool,
    serving: AtomicBool,
    /// The membership clock's nanoseconds until which new work may be admitted; 0 before any
    /// lease.
    serve_until: AtomicU64,
    pub(super) view: watch::Sender<NodeView>,
    #[cfg(feature = "test-faults")]
    pub(super) paused: watch::Sender<bool>,
    /// Test seam: the membership clock's nanoseconds at the last successful renewal's send.
    #[cfg(feature = "test-faults")]
    last_sent: AtomicU64,
    /// Test seam: the purge's lock timeout in milliseconds (0: the built-in one).
    #[cfg(feature = "test-faults")]
    purge_lock_ms: AtomicU64,
}

impl Shared {
    pub(super) fn new(database: Arc<str>, identity: Arc<NodeIdentity>, view: NodeView) -> Self {
        Self {
            database,
            identity,
            leaving: AtomicBool::new(false),
            serving: AtomicBool::new(false),
            serve_until: AtomicU64::new(0),
            view: watch::Sender::new(view),
            #[cfg(feature = "test-faults")]
            paused: watch::Sender::new(false),
            #[cfg(feature = "test-faults")]
            last_sent: AtomicU64::new(0),
            #[cfg(feature = "test-faults")]
            purge_lock_ms: AtomicU64::new(0),
        }
    }

    /// Test seam: record the send of the last successful renewal.
    #[cfg(feature = "test-faults")]
    pub(super) fn set_last_sent(&self, at: Tick) {
        self.last_sent.store(at.as_nanos(), Ordering::Release);
    }

    /// Test seam: how long the purge waits for a row lock instead of `built_in`.
    #[cfg(feature = "test-faults")]
    pub(super) fn purge_lock_timeout(&self, built_in: Duration) -> Duration {
        match self.purge_lock_ms.load(Ordering::Acquire) {
            0 => built_in,
            ms => Duration::from_millis(ms),
        }
    }

    pub(super) fn set_serve_until(&self, at: Tick) {
        self.serve_until.store(at.as_nanos(), Ordering::Release);
    }

    pub(super) fn set_serving(&self, on: bool) {
        self.serving.store(on, Ordering::Release);
    }

    /// The owner drains or shuts down: admission stops now and for good, whatever the task does
    /// next (design §6.4), and every reader of the view (a kept handle's `view()`, a subscriber)
    /// sees the drain at once. The flag is set under the view's lock, so a view the task publishes
    /// at the same moment is either changed here or sees the flag in [`publish`](Self::publish).
    pub(super) fn leave(&self) {
        self.view.send_modify(|view| {
            self.leaving.store(true, Ordering::Release);
            self.set_serving(false);
            leaving_view(view);
        });
    }

    pub(super) fn is_serving(&self) -> bool {
        !self.leaving.load(Ordering::Acquire)
            && self.serving.load(Ordering::Acquire)
            && clock::now().as_nanos() < self.serve_until.load(Ordering::Acquire)
    }

    pub(super) fn deadline_passed(&self) -> bool {
        clock::now().as_nanos() >= self.serve_until.load(Ordering::Acquire)
    }

    /// Publish the task's view; once the owner asked to leave, as a draining one (work the task
    /// started before, a transition that decided `Ready`, never shows as serving).
    pub(super) fn publish(&self, mut view: NodeView) {
        self.view.send_modify(|current| {
            if self.leaving.load(Ordering::Acquire) {
                leaving_view(&mut view);
            }
            *current = view;
        });
    }

    /// The heartbeat task ended (`panicked`, or dropped by an abort): admission stops at once and
    /// the view says so, unless the membership was shut down.
    pub(super) fn task_ended(&self, panicked: bool) {
        self.set_serving(false);
        self.view.send_if_modified(|view| {
            if view.readiness == Readiness::Stopped {
                return false;
            }
            view.readiness = Readiness::Unready(UnreadyReason::HeartbeatEnded { panicked });
            view.counted = false;
            true
        });
        if panicked {
            tracing::error!(target: "erp::node", database = %self.database,
                node = %self.identity.id(),
                "the heartbeat task panicked: new work is refused; restart the process");
        }
    }

    fn readiness(&self) -> Readiness {
        // a drain is in the view already (`leave`, `publish`)
        let view = self.view.borrow();
        match view.readiness() {
            Readiness::Ready if self.deadline_passed() => {
                Readiness::Unready(UnreadyReason::HeartbeatLost {
                    last_error: view.last_error().cloned(),
                })
            }
            other => other.clone(),
        }
    }
}

/// A view of a membership whose owner asked to leave: what the task started before (starting,
/// settling, a transition to ready) reads as draining; stopped and unready views stay as they are.
fn leaving_view(view: &mut NodeView) {
    if matches!(
        view.readiness,
        Readiness::Starting | Readiness::Settling { .. } | Readiness::Ready
    ) {
        view.readiness = Readiness::Unready(UnreadyReason::Draining);
    }
}

/// The bound on a command to the heartbeat task: it may be in a round when the command arrives
/// (a renewal, the details write, the transition, each bounded by the interval plus its grace; a
/// purge in progress is cut short by the command) and then runs the
/// command's own work (a transition, or for a drain a renewal and the details write): five times
/// the interval plus its grace.
fn command_bound(options: &super::NodeOptions) -> Duration {
    heartbeat::with_grace(options.interval())
        .saturating_mul(5)
        .saturating_add(Duration::from_secs(2))
}

/// What request handling needs from a membership: admission, readiness, the view. Cheap to clone
/// (one `Arc`) and `Send + Sync`, so every request task holds one, while the
/// [`NodeMembership`] owner handle stays unique and can [`shutdown`](NodeMembership::shutdown).
#[derive(Clone)]
pub struct NodeAdmission {
    shared: Arc<Shared>,
    origin: AdmissionOrigin,
}

/// Kept separately from identity: a detached seam may hold the very same identity as a real row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionOrigin {
    Registered,
    Detached,
}

impl std::fmt::Debug for NodeAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeAdmission")
            .field("database", &self.shared.database)
            .field("node", &self.shared.identity.id())
            .finish_non_exhaustive()
    }
}

impl NodeAdmission {
    /// Only an actual registration with this process's membership claim grants binding origin.
    pub(crate) fn is_registered(&self) -> bool {
        self.origin == AdmissionOrigin::Registered
    }

    /// The immutable identity of this admission, not a caller-provided node/digest pair.
    pub(crate) fn identity(&self) -> &Arc<NodeIdentity> {
        &self.shared.identity
    }

    /// Benchmark seam: a handle with no membership behind it, ready and admitting for
    /// `serve_for` from now on the membership clock (`crates/erp-orm/benches/node_admission.rs`).
    #[cfg(any(feature = "test-faults", feature = "bench-seams"))]
    #[doc(hidden)]
    pub fn detached_for_benches(
        identity: Arc<NodeIdentity>,
        database: &str,
        serve_for: Duration,
    ) -> Self {
        let database: Arc<str> = database.into();
        let until = clock::now().saturating_add(serve_for);
        let view = heartbeat::detached_view(&database, &identity, until);
        let shared = Shared::new(database, identity, view);
        shared.set_serve_until(until);
        shared.set_serving(true);
        Self {
            shared: Arc::new(shared),
            origin: AdmissionOrigin::Detached,
        }
    }

    /// The database.
    pub fn database(&self) -> &str {
        &self.shared.database
    }

    /// Whether a new request may be admitted now: ready, and the self-fence deadline of the last
    /// successful renewal not passed, and the owner has not asked to drain or shut down. Three
    /// atomic loads and one clock read (a monotonic clock that counts suspend; a vDSO call on
    /// Linux); it does not depend on the heartbeat task having run.
    pub fn is_serving(&self) -> bool {
        self.shared.is_serving()
    }

    /// The readiness, with the self-fence deadline applied: a published `Ready` whose deadline
    /// passed is reported as [`UnreadyReason::HeartbeatLost`].
    pub fn readiness(&self) -> Readiness {
        self.shared.readiness()
    }

    /// The last published view. Its readiness is the published one: a drain or shutdown shows at
    /// once, the self-fence deadline only in [`readiness`](Self::readiness) (and in `divisor`).
    pub fn view(&self) -> NodeView {
        self.shared.view.borrow().clone()
    }

    /// Changes of the view (after every renewal and transition, and at the end).
    pub fn subscribe(&self) -> watch::Receiver<NodeView> {
        self.shared.view.subscribe()
    }

    /// Test seam: pause or resume the heartbeat task from a request handle (the owner handle may
    /// be inside [`NodeMembership::shutdown`]).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn pause_heartbeat_for_tests(&self, paused: bool) {
        self.shared.paused.send_replace(paused);
    }
}

/// One process's registration in one database: its `erp_node` row, the
/// heartbeat task that renews it, and its readiness.
///
/// This owner handle is unique: request tasks share a [`NodeAdmission`]
/// ([`admission`](Self::admission)), so the owner can still call [`shutdown`](Self::shutdown) to
/// leave. Dropping it stops the heartbeat without writing anything (as a crash would): the row
/// then expires after its lease, and every admission handle stops admitting at once.
///
/// ```no_run
/// # async fn example(identity: std::sync::Arc<erp_orm::node::NodeIdentity>,
/// #                  heartbeat_pool: deadpool_postgres::Pool) -> Result<(), erp_orm::node::NodeError> {
/// use erp_orm::node::{NodeMembership, NodeRole};
/// let membership = NodeMembership::register(identity, heartbeat_pool).await?;
/// // … load the registry, then:
/// let readiness = membership.request_ready().await?;
/// // per request task:
/// let admission = membership.admission();
/// if admission.is_serving() { /* admit */ }
/// let share = 1000 / admission.view().divisor(NodeRole::App).get();
/// membership.drain().await?;
/// membership.shutdown().await?;
/// # let _ = (readiness, share);
/// # Ok(())
/// # }
/// ```
pub struct NodeMembership {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
    task: Option<JoinHandle<()>>,
    /// This process's one membership of the identity in this database, given back when this
    /// handle is dropped (`None` only for the test seam that stands for another process).
    _claim: Option<MembershipClaim>,
}

impl std::fmt::Debug for NodeMembership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeMembership")
            .field("database", &self.shared.database)
            .field("node", &self.shared.identity.id())
            .finish_non_exhaustive()
    }
}

impl NodeMembership {
    /// Register in the database of `pool` and start the heartbeat. The row is written as
    /// `starting`; readiness is requested separately ([`request_ready`](Self::request_ready)).
    ///
    /// `pool` is the heartbeat's own pool ([`heartbeat_pool`](super::heartbeat_pool)): request
    /// transactions never draw from it (one connection is enough, and it may go through a
    /// transaction pooler; design §6.5). The heartbeat role needs the privileges of
    /// [`Transaction::grant_node_role`](crate::Transaction::grant_node_role).
    ///
    /// A `stopped` or expired row of this identity is replaced; a fresh `starting` row that an
    /// earlier registration of this process committed although its answer was lost is adopted.
    ///
    /// # Errors
    ///
    /// [`NodeError::TableMissing`] when the schema sync did not run;
    /// [`NodeError::AlreadyRegistered`] when this process still holds a membership of this
    /// identity in that database (its owner handle was not dropped yet), or when the identity has
    /// a fresh row there that is not `stopped` (one membership per process and database);
    /// [`NodeError::DevOnLiveCluster`] for a dev identity where non-dev nodes are live;
    /// [`NodeError::Timeout`] and [`NodeError::Database`] for database failures.
    pub async fn register(
        identity: Arc<NodeIdentity>,
        pool: Pool,
    ) -> Result<NodeMembership, NodeError> {
        let (shared, state, claim) = heartbeat::register(identity, &pool, None).await?;
        Ok(Self::spawn(Arc::new(shared), pool, state, Some(claim)))
    }

    /// Test seam: [`register`](Self::register) with a fault injected.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub async fn register_with_fault_for_tests(
        identity: Arc<NodeIdentity>,
        pool: Pool,
        fault: RegisterFault,
    ) -> Result<NodeMembership, NodeError> {
        let (shared, state, claim) = heartbeat::register(identity, &pool, Some(fault)).await?;
        Ok(Self::spawn(Arc::new(shared), pool, state, Some(claim)))
    }

    fn spawn(
        shared: Arc<Shared>,
        pool: Pool,
        state: heartbeat::State,
        claim: Option<MembershipClaim>,
    ) -> Self {
        let (commands, rx) = mpsc::channel(8);
        let task = tokio::spawn(heartbeat::Task::new(Arc::clone(&shared), pool, rx, state).run());
        Self {
            shared,
            commands,
            task: Some(task),
            _claim: claim,
        }
    }

    /// The database.
    pub fn database(&self) -> &str {
        &self.shared.database
    }

    /// The identity.
    pub fn identity(&self) -> &Arc<NodeIdentity> {
        &self.shared.identity
    }

    /// A handle for request tasks: admission, readiness and the view.
    pub fn admission(&self) -> NodeAdmission {
        NodeAdmission {
            shared: Arc::clone(&self.shared),
            origin: if self._claim.is_some() {
                AdmissionOrigin::Registered
            } else {
                // The restored-process test seam owns no real registration claim.
                AdmissionOrigin::Detached
            },
        }
    }

    /// As [`NodeAdmission::is_serving`].
    pub fn is_serving(&self) -> bool {
        self.shared.is_serving()
    }

    /// As [`NodeAdmission::readiness`].
    pub fn readiness(&self) -> Readiness {
        self.shared.readiness()
    }

    /// The last published view. Its readiness is the published one: a drain or shutdown shows at
    /// once, the self-fence deadline only in [`readiness`](Self::readiness) (and in `divisor`).
    pub fn view(&self) -> NodeView {
        self.shared.view.borrow().clone()
    }

    /// Changes of the view (after every renewal and transition).
    pub fn subscribe(&self) -> watch::Receiver<NodeView> {
        self.shared.view.subscribe()
    }

    fn command_bound(&self) -> Duration {
        command_bound(self.shared.identity.options())
    }

    async fn ask<T>(
        &self,
        what: &'static str,
        make: impl FnOnce(oneshot::Sender<Result<T, NodeError>>) -> Command,
    ) -> Result<T, NodeError> {
        let ended = || NodeError::MembershipEnded {
            database: self.shared.database.to_string(),
        };
        let bound = self.command_bound();
        let timed_out = || NodeError::Timeout {
            database: self.shared.database.to_string(),
            what,
            after: bound,
        };
        // one bound for the whole exchange: a full queue (a starved task) counts too
        let deadline = tokio::time::Instant::now() + bound;
        let (tx, rx) = oneshot::channel();
        match tokio::time::timeout_at(deadline, self.commands.send(make(tx))).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(ended()),
            Err(_) => return Err(timed_out()),
        }
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(ended()),
            Err(_) => Err(timed_out()),
        }
    }

    /// Run the readiness transition (design §7.2) and return its outcome. From now on the
    /// heartbeat re-runs it after every successful renewal until the membership is ready.
    ///
    /// # Errors
    ///
    /// [`NodeError::IdentityConflict`] (sticky; the server exits); [`NodeError::Timeout`] when the
    /// heartbeat task does not answer (it is starved); [`NodeError::Database`] when the
    /// transition's transaction fails (it is retried after the next renewal).
    pub async fn request_ready(&self) -> Result<Readiness, NodeError> {
        self.ask("the readiness request", Command::RequestReady)
            .await
    }

    /// Start draining: [`Readiness::Unready`] with [`UnreadyReason::Draining`] at once (admission
    /// stops), the row written as `stopping` (out of every divisor), heartbeats go on until
    /// [`shutdown`](Self::shutdown). A drain is final: work the heartbeat task started before it
    /// (a readiness transition) and later readiness requests never turn admission back on;
    /// register a new membership to serve again.
    ///
    /// # Errors
    ///
    /// [`NodeError::Heartbeat`] when the `stopping` row could not be written (the node is
    /// draining anyway, and the heartbeat retries); [`NodeError::Timeout`];
    /// [`NodeError::MembershipEnded`].
    pub async fn drain(&self) -> Result<(), NodeError> {
        self.shared.leave();
        self.ask("the drain", Command::Drain).await
    }

    /// Leave: state `stopped` and `expires_at = now()`, then stop the heartbeat. The readiness is
    /// [`Readiness::Stopped`] afterwards, also when the write failed.
    ///
    /// # Errors
    ///
    /// [`NodeError::IdentityConflict`] when another process renews this node id (its row is ahead
    /// of this process; nothing is written); [`NodeError::Database`] when the row could not be
    /// written (it expires after its lease); [`NodeError::Timeout`];
    /// [`NodeError::MembershipEnded`].
    pub async fn shutdown(mut self) -> Result<(), NodeError> {
        self.shared.leave();
        let answer = self.ask("the shutdown", Command::Shutdown).await;
        if let Some(task) = self.task.take() {
            if answer.is_ok() {
                if let Err(e) = task.await {
                    tracing::error!(target: "erp::node", database = %self.shared.database,
                        error = %e, "the heartbeat task failed");
                }
            } else {
                task.abort();
            }
        }
        answer
    }

    /// Test seam: run one renewal now (and the pending transition), return the view.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub async fn renew_now_for_tests(&self) -> Result<NodeView, NodeError> {
        self.ask("the renewal", Command::RenewNow).await
    }

    /// Test seam: when the last successful renewal was sent and how long until the self-fence.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn fence_timing_for_tests(&self) -> FenceTiming {
        let now = clock::now().as_nanos();
        let sent = self.shared.last_sent.load(Ordering::Acquire);
        let until = self.shared.serve_until.load(Ordering::Acquire);
        FenceTiming {
            since_sent: Duration::from_nanos(now.saturating_sub(sent)),
            until_fence: (until > now).then(|| Duration::from_nanos(until - now)),
        }
    }

    /// Test seam: make the heartbeat task panic (no answer is sent).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub async fn panic_heartbeat_for_tests(&self) -> Result<(), NodeError> {
        self.commands
            .send(Command::PanicForTests)
            .await
            .map_err(|_| NodeError::MembershipEnded {
                database: self.shared.database.to_string(),
            })
    }

    /// Test seam: pause or resume the heartbeat task (a starved runtime).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn pause_heartbeat_for_tests(&self, paused: bool) {
        self.shared.paused.send_replace(paused);
    }

    /// Test seam: how long the purge waits for a row lock (an operator's transaction) before it
    /// gives up for this turn, instead of the built-in 100 ms: long enough for a test to act while
    /// the purge waits.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn purge_lock_timeout_for_tests(&self, timeout: Duration) {
        let ms = u64::try_from(timeout.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        self.shared.purge_lock_ms.store(ms, Ordering::Release);
    }

    /// Test seam: a copy of this process's membership state running on its own task and pool,
    /// with the same node id and beat (a restored VM snapshot or container checkpoint).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub async fn clone_process_for_tests(&self, pool: Pool) -> Result<NodeMembership, NodeError> {
        let state = self.ask("the snapshot", Command::Snapshot).await?;
        let shared = Shared::new(
            Arc::clone(&self.shared.database),
            Arc::clone(&self.shared.identity),
            self.view(),
        );
        if let Some(at) = state.serve_until() {
            shared.set_serve_until(at);
        }
        shared.set_serving(self.shared.serving.load(Ordering::Acquire));
        // it stands for another process: it holds no claim of this one
        Ok(Self::spawn(Arc::new(shared), pool, state, None))
    }
}

impl Drop for NodeMembership {
    fn drop(&mut self) {
        let Some(mut task) = self.task.take() else {
            return;
        };
        if task.is_finished() {
            // the task ended on its own: say how (a panic's message is in its `JoinError`)
            let mut cx = Context::from_waker(Waker::noop());
            if let Poll::Ready(Err(e)) = std::pin::Pin::new(&mut task).poll(&mut cx) {
                tracing::error!(target: "erp::node", database = %self.shared.database,
                    node = %self.shared.identity.id(), error = %e,
                    "the heartbeat task failed");
            }
            return;
        }
        tracing::warn!(
            target: "erp::node",
            database = %self.shared.database,
            node = %self.shared.identity.id(),
            "node membership dropped without shutdown: its row expires after its lease"
        );
        self.shared.leave();
        task.abort();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use crate::node::NodeOptions;

    /// Not a spec case: a command waits as long as the task's worst case — the round's renewal,
    /// details write and transition, then the command's own transition (or a drain's renewal and
    /// details write), each bounded by the interval plus its grace — and two seconds more.
    #[test]
    fn the_command_bound_covers_the_task_worst_case() {
        let options = NodeOptions::default();
        let one = heartbeat::with_grace(options.interval());
        assert_eq!(
            command_bound(&options),
            one.saturating_mul(5).saturating_add(Duration::from_secs(2))
        );
    }
}
