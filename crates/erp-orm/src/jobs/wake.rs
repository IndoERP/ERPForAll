//! Wake-ups (design §5.7): how an enqueue tells the runners a job is due without
//! spending the cluster's NOTIFY budget at the enqueue rate.
//!
//! An enqueue whose job is due within `jobs.wake_slack` pokes the runners of its own process
//! after the commit, and sends one payload-free `NOTIFY erp_jobs` before it unless this process
//! sent one for the database within `jobs.notify_interval` (the time is recorded at the commit, so
//! a rolled-back NOTIFY suppresses nothing) or the node registry shows no other node. A lost or
//! suppressed hint costs latency, never a job: every runner also polls.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use super::runner::{Inner, NodeViewFn};

/// The channel job enqueues notify on.
pub const JOBS_CHANNEL: &str = "erp_jobs";

struct Local {
    orm_id: u64,
    inner: Weak<Inner>,
    node: Option<NodeViewFn>,
}

static LOCAL: Mutex<Vec<Local>> = Mutex::new(Vec::new());
static LAST_NOTIFY: Mutex<Option<HashMap<u64, Instant>>> = Mutex::new(None);

/// A runner of the ORM lineage `orm_id` exists in this process: enqueues of that lineage wake it.
pub(crate) fn register(orm_id: u64, inner: &Arc<Inner>) {
    let mut local = LOCAL.lock().unwrap_or_else(PoisonError::into_inner);
    local.retain(|l| l.inner.strong_count() > 0);
    local.push(Local {
        orm_id,
        inner: Arc::downgrade(inner),
        node: inner.node.clone(),
    });
}

/// Poke the runners of the lineage `orm_id` in this process.
pub(crate) fn wake_local(orm_id: u64) {
    let local = LOCAL.lock().unwrap_or_else(PoisonError::into_inner);
    for runner in local.iter().filter(|l| l.orm_id == orm_id) {
        if let Some(inner) = runner.inner.upgrade() {
            inner.wake.notify_one();
        }
    }
}

/// Whether an enqueue should send the NOTIFY now: not within `interval` of the last one this
/// process sent for the lineage, and not when every runner this process has for it sees no other
/// node in the registry (the poke already reached the only node there is). A process with no
/// runner of its own for the database (an `app` node that enqueues for dedicated workers)
/// always may.
pub(crate) fn should_notify(orm_id: u64, interval: Duration) -> bool {
    if let Some(last) = LAST_NOTIFY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|m| m.get(&orm_id))
        && last.elapsed() < interval
    {
        return false;
    }
    let local = LOCAL.lock().unwrap_or_else(PoisonError::into_inner);
    let mut views = local
        .iter()
        .filter(|l| l.orm_id == orm_id && l.inner.strong_count() > 0)
        .peekable();
    if views.peek().is_none() {
        return true;
    }
    // a runner without a registry view cannot know it is alone: it does not suppress
    views.any(|l| match &l.node {
        Some(nodes) => nodes.has_peers(),
        None => true,
    })
}

/// A NOTIFY of the lineage `orm_id` committed now.
pub(crate) fn mark_notified(orm_id: u64) {
    LAST_NOTIFY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(orm_id, Instant::now());
}

/// Forget the last NOTIFY of the lineage (tests).
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub fn reset_notify_state_for_tests(orm_id: u64) {
    if let Some(map) = LAST_NOTIFY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        map.remove(&orm_id);
    }
}
