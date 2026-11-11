//! The admission budget of one sanitising call.
//!
//! [`HtmlBudget`] is the [`Budget`] handed to the vendored html5ever tokenizer and tree builder
//! and used by the arena sink ([`crate::dom::DomSink`]). It counts retained bytes, work units,
//! DOM nodes and the open-element depth against the call's [`Limits`]. All sums are saturating, so
//! an enormous request is refused instead of wrapping, and the first refusal is sticky: every
//! later request is refused with that same first error.

use std::mem::size_of;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use html5ever::budget::{Budget, BudgetExceeded, BudgetKind};

use crate::{HtmlSanitizeError, Limits};

#[derive(Default)]
struct Counters {
    live_bytes: u64,
    peak_bytes: u64,
    work: u64,
    nodes: u64,
    max_depth: u64,
    /// The deepest open-element stack of the current pass (reset by `begin_next_pass`).
    pass_depth: u64,
    /// The work counter when the current pass began (set by `begin_next_pass`).
    pass_start_work: u64,
    tripped: Option<BudgetExceeded>,
    /// An inconsistency inside the sanitiser (not a limit the input exceeded); the text is fixed.
    fault: Option<&'static str>,
    /// Admission calls seen, for the refuse-at-k sweep.
    #[cfg(test)]
    calls: u64,
    /// Refuse the call with this number (1-based), whatever it asks for.
    #[cfg(test)]
    refuse_call: Option<u64>,
}

/// The budget of one call. Shared (`Arc`) between the tokenizer, the tree builder and the sink,
/// so its state is behind a lock.
pub(crate) struct HtmlBudget {
    bytes: u64,
    work: u64,
    depth: u64,
    nodes: u64,
    state: Mutex<Counters>,
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Record `kind` as the first refusal unless one is already recorded; return the first one.
fn trip(st: &mut Counters, kind: BudgetKind, requested: u64, limit: u64) -> BudgetExceeded {
    *st.tripped
        .get_or_insert(BudgetExceeded::new(kind, requested, limit))
}

impl HtmlBudget {
    /// A fresh budget with nothing charged.
    pub(crate) fn new(limits: &Limits) -> Arc<HtmlBudget> {
        Arc::new(HtmlBudget {
            bytes: limits.working_bytes(),
            work: limits.work_units(),
            depth: limits.depth(),
            nodes: limits.nodes(),
            state: Mutex::new(Counters::default()),
        })
    }

    // A poisoned lock only means another thread panicked while holding plain counters; they are
    // still consistent, so carry on instead of panicking inside the parser.
    fn lock(&self) -> MutexGuard<'_, Counters> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Common start of every admission: the sticky refusal, and the test-only sweep hook.
    fn enter(&self, st: &mut Counters, kind: BudgetKind, limit: u64) -> Result<(), BudgetExceeded> {
        if let Some(first) = st.tripped {
            return Err(first);
        }
        #[cfg(test)]
        {
            st.calls += 1;
            if st.refuse_call == Some(st.calls) {
                return Err(trip(st, kind, limit.saturating_add(1), limit));
            }
        }
        #[cfg(not(test))]
        let _ = (kind, limit);
        Ok(())
    }

    /// Record an inconsistency inside the sanitiser. It stops the parse like a refusal and
    /// surfaces as [`HtmlSanitizeError::Internal`] with `reason`, never as a limit.
    pub(crate) fn fail(&self, reason: &'static str) {
        let mut st = self.lock();
        st.fault.get_or_insert(reason);
        trip(&mut st, BudgetKind::Accounting, 0, 0);
    }

    /// The error to report if the call must stop now: an internal fault, or the first refusal.
    pub(crate) fn failure(&self) -> Option<HtmlSanitizeError> {
        let st = self.lock();
        if let Some(reason) = st.fault {
            return Some(HtmlSanitizeError::Internal { reason });
        }
        st.tripped.map(map_budget_error)
    }

    /// True once any refusal or fault has been recorded.
    pub(crate) fn refused(&self) -> bool {
        self.lock().tripped.is_some()
    }

    /// The error for a refused request: an internal fault recorded here wins, else the refusal.
    fn refusal(&self, refused: BudgetExceeded) -> HtmlSanitizeError {
        self.failure().unwrap_or_else(|| map_budget_error(refused))
    }

    /// Start the next pass over the output of the previous one (the reparse gate).
    ///
    /// The previous pass's tree and scratch buffers are gone, so the node count restarts from
    /// zero and the live bytes drop to `retained` (the previous output, which the caller still
    /// holds). The work counter, the peak and the sticky refusal carry on: work is the cost of the
    /// whole call. The depth limit is not widened: the first pass refuses input whose output would
    /// be deeper than the limit (see `admit_wrapper_level`), so the second pass reads output that
    /// already fits.
    pub(crate) fn begin_next_pass(&self, retained: usize) {
        let mut st = self.lock();
        st.live_bytes = st.live_bytes.min(to_u64(retained));
        st.nodes = 0;
        st.pass_depth = 0;
        st.pass_start_work = st.work;
    }

    /// Require that the output could be sanitised again within the same work limit (
    /// `sanitize(y)` must succeed whenever `sanitize(x)` returned `y`).
    ///
    /// Work depends only on the text a pass reads, and the second pass reads `y`, so a call on `y`
    /// costs exactly twice what the second pass of the call on `x` cost. That call on `x` charged
    /// the first pass as well, and the first pass can be cheaper than the second (output longer
    /// than input), so the call on `x` could pass while the call on `y` ran out. Called after the
    /// second pass: the work of that pass, counted twice, must fit the limit, which makes
    /// `sanitize(y)` fit whenever this returns `Ok`. This is a check after the work was done, so a
    /// call may use up to the limit and still be refused with the configured limit. The memory
    /// limit has no such guarantee (not measured): its second pass starts from the capacity of the
    /// first output.
    pub(crate) fn admit_repeat_work(&self) -> Result<(), HtmlSanitizeError> {
        let mut st = self.lock();
        let second = st.work.saturating_sub(st.pass_start_work);
        let want = second.saturating_mul(2);
        if want > self.work {
            let refused = trip(&mut st, BudgetKind::Work, want, self.work);
            drop(st);
            return Err(self.refusal(refused));
        }
        Ok(())
    }

    /// Admit one more wrapper level above everything this pass parsed (the leading-text `<p>`).
    /// The output is read back as input by the reparse gate and by every later call, so it must
    /// never be deeper than the limit: if the deepest stack of this pass plus the wrapper exceeds
    /// it, the call is refused with the configured limit. The count is of the parsed tree, before
    /// cleaning, so it is conservative (fails closed).
    pub(crate) fn admit_wrapper_level(&self) -> Result<(), HtmlSanitizeError> {
        let mut st = self.lock();
        let want = st.pass_depth.saturating_add(1);
        if want > self.depth {
            let refused = trip(&mut st, BudgetKind::Depth, want, self.depth);
            drop(st);
            return Err(self.refusal(refused));
        }
        Ok(())
    }

    /// Charge `n` retained bytes, before the allocation that needs them.
    pub(crate) fn charge_bytes(&self, n: usize) -> Result<(), HtmlSanitizeError> {
        self.admit_bytes(n).map_err(|e| self.refusal(e))
    }

    /// Charge `n` work units.
    pub(crate) fn charge_work(&self, n: usize) -> Result<(), HtmlSanitizeError> {
        self.admit_work(to_u64(n)).map_err(|e| self.refusal(e))
    }

    /// Make room for `additional` more elements in `v`: charge the capacity it will gain, then
    /// reserve exactly that. The charge comes first, so a refused growth allocates nothing.
    pub(crate) fn reserve<T>(
        &self,
        v: &mut Vec<T>,
        additional: usize,
    ) -> Result<(), HtmlSanitizeError> {
        if v.capacity() - v.len() >= additional {
            return Ok(());
        }
        let needed = v.len().saturating_add(additional);
        let new_cap = needed.max(v.capacity().saturating_mul(2)).max(4);
        self.charge_bytes((new_cap - v.capacity()).saturating_mul(size_of::<T>()))?;
        if v.try_reserve_exact(new_cap - v.len()).is_err() {
            self.fail("allocation failed");
            return Err(self.failure().unwrap_or(HtmlSanitizeError::Internal {
                reason: "allocation failed",
            }));
        }
        Ok(())
    }

    /// Nodes admitted so far.
    #[cfg(test)]
    pub(crate) fn nodes(&self) -> u64 {
        self.lock().nodes
    }

    /// Work units admitted so far.
    #[cfg(test)]
    pub(crate) fn work(&self) -> u64 {
        self.lock().work
    }

    /// Work units the current (second) pass admitted.
    #[cfg(test)]
    pub(crate) fn last_pass_work(&self) -> u64 {
        let st = self.lock();
        st.work.saturating_sub(st.pass_start_work)
    }

    /// The highest number of live charged bytes seen.
    #[cfg(test)]
    pub(crate) fn peak_bytes(&self) -> u64 {
        self.lock().peak_bytes
    }

    /// The deepest open-element stack that was admitted.
    #[cfg(test)]
    pub(crate) fn max_depth(&self) -> u64 {
        self.lock().max_depth
    }

    /// Admission calls seen so far.
    #[cfg(test)]
    pub(crate) fn calls(&self) -> u64 {
        self.lock().calls
    }

    /// Refuse the `k`-th admission call (1-based) with the kind that call asked for.
    #[cfg(test)]
    pub(crate) fn refuse_call(&self, k: u64) {
        self.lock().refuse_call = Some(k);
    }
}

impl Budget for HtmlBudget {
    fn admit_bytes(&self, n: usize) -> Result<(), BudgetExceeded> {
        let mut st = self.lock();
        self.enter(&mut st, BudgetKind::WorkingBytes, self.bytes)?;
        let want = st.live_bytes.saturating_add(to_u64(n));
        if want > self.bytes {
            return Err(trip(&mut st, BudgetKind::WorkingBytes, want, self.bytes));
        }
        st.live_bytes = want;
        st.peak_bytes = st.peak_bytes.max(want);
        Ok(())
    }

    // Releasing more than is live means a charge/release pair is out of balance: a bug in the
    // parser's accounting. It is never hidden. It is recorded as the sticky first refusal
    // (`Accounting`, reported as an internal error), in every build profile. After another
    // refusal a refused charge may legitimately be released, so that case only clamps at zero.
    fn release_bytes(&self, n: usize) {
        let mut st = self.lock();
        let n = to_u64(n);
        if n > st.live_bytes {
            if st.tripped.is_none() {
                let live = st.live_bytes;
                trip(&mut st, BudgetKind::Accounting, n, live);
            }
            st.live_bytes = 0;
            return;
        }
        st.live_bytes -= n;
    }

    fn admit_work(&self, n: u64) -> Result<(), BudgetExceeded> {
        let mut st = self.lock();
        self.enter(&mut st, BudgetKind::Work, self.work)?;
        let want = st.work.saturating_add(n);
        if want > self.work {
            return Err(trip(&mut st, BudgetKind::Work, want, self.work));
        }
        st.work = want;
        Ok(())
    }

    fn admit_depth(&self, depth: usize) -> Result<(), BudgetExceeded> {
        let mut st = self.lock();
        self.enter(&mut st, BudgetKind::Depth, self.depth)?;
        let depth = to_u64(depth);
        if depth > self.depth {
            return Err(trip(&mut st, BudgetKind::Depth, depth, self.depth));
        }
        st.max_depth = st.max_depth.max(depth);
        st.pass_depth = st.pass_depth.max(depth);
        Ok(())
    }

    fn admit_node(&self) -> Result<(), BudgetExceeded> {
        let mut st = self.lock();
        self.enter(&mut st, BudgetKind::Nodes, self.nodes)?;
        let want = st.nodes.saturating_add(1);
        if want > self.nodes {
            return Err(trip(&mut st, BudgetKind::Nodes, want, self.nodes));
        }
        st.nodes = want;
        Ok(())
    }

    fn tripped(&self) -> Option<BudgetExceeded> {
        self.lock().tripped
    }
}

/// Turn a budget refusal into the sanitiser's error. A limit becomes its own error carrying the
/// configured limit. An accounting fault, or a kind this crate does not know yet, is an internal
/// failure and is never presented as a limit the document exceeded.
pub(crate) fn map_budget_error(refusal: BudgetExceeded) -> HtmlSanitizeError {
    let limit = refusal.limit;
    match refusal.kind {
        BudgetKind::Depth => HtmlSanitizeError::DepthExceeded { limit },
        BudgetKind::Nodes => HtmlSanitizeError::NodeLimit { limit },
        BudgetKind::Work => HtmlSanitizeError::WorkLimit { limit },
        BudgetKind::WorkingBytes => HtmlSanitizeError::MemoryLimit { limit },
        BudgetKind::Accounting => HtmlSanitizeError::Internal {
            reason: "budget accounting fault",
        },
        // `BudgetKind` is non-exhaustive: a kind added later is an internal failure until it is
        // listed above on purpose.
        _ => HtmlSanitizeError::Internal {
            reason: "unknown budget kind",
        },
    }
}
