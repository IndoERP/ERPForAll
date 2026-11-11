//! Shared admission for simultaneously retained description-construction storage.
use crate::{LimitKind, ViewError, ViewResult};
use std::mem::size_of;
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicUsize, Ordering},
};

const ACTIVE: u8 = 0;
const BYTE_LIMIT_REFUSAL: u8 = 1;
const CONSTRUCTION_REFUSAL: u8 = 2;
const WORK_LIMIT_REFUSAL: u8 = 3;
const EXPRESSION_BYTES_REFUSAL: u8 = 4;

const MAX_BUILD_BYTES: usize = crate::ViewLimits::DEFAULT.get(LimitKind::DescriptionBuildBytes);
const MAX_EXPRESSION_WORK: usize =
    crate::ViewLimits::DEFAULT.get(LimitKind::DescriptionExpressionWork);
const MAX_EXPRESSION_BYTES: usize =
    crate::ViewLimits::DEFAULT.get(LimitKind::DescriptionExpressionBytes);

#[derive(Debug)]
struct State {
    limit: usize,
    used: AtomicUsize,
    /// Expression work steps and aggregate expression bytes only ever accumulate: unlike the
    /// retained-storage counter nothing releases them, so no value, view or visitor resets them.
    work_limit: usize,
    work: AtomicUsize,
    expression_bytes_limit: usize,
    expression_bytes: AtomicUsize,
    failure: AtomicU8,
}

/// One request's sticky construction admission. Clones share the same allowance.
///
/// Reservations must precede allocation and remain owned alongside the retained storage.
/// This limits logical owned storage; it is not a product allocation/performance measurement.
#[derive(Debug, Clone)]
pub struct DescriptionBudget {
    state: Arc<State>,
}

impl DescriptionBudget {
    /// The mandatory 32 MiB construction ceiling, independently of encoded-response limits, with
    /// the default request-wide expression work (1,000,000 steps) and bytes (16 MiB) allowances.
    pub fn new() -> Self {
        Self::build(MAX_BUILD_BYTES, MAX_EXPRESSION_WORK, MAX_EXPRESSION_BYTES)
    }
    /// Lower the construction ceiling. Zero or a value above 32 MiB is refused.
    pub fn with_limit(limit: usize) -> ViewResult<Self> {
        Self::with_limits(limit, MAX_EXPRESSION_WORK, MAX_EXPRESSION_BYTES)
    }
    /// Lower the construction ceiling and the request-wide expression work and byte allowances.
    /// Each is lower-only: zero or a value above its default ceiling is refused.
    pub fn with_limits(
        build_bytes: usize,
        expression_work: usize,
        expression_bytes: usize,
    ) -> ViewResult<Self> {
        for (limit, value, max) in [
            (
                LimitKind::DescriptionBuildBytes,
                build_bytes,
                MAX_BUILD_BYTES,
            ),
            (
                LimitKind::DescriptionExpressionWork,
                expression_work,
                MAX_EXPRESSION_WORK,
            ),
            (
                LimitKind::DescriptionExpressionBytes,
                expression_bytes,
                MAX_EXPRESSION_BYTES,
            ),
        ] {
            if value == 0 || value > max {
                return Err(ViewError::InvalidLimit { limit, value, max });
            }
        }
        Ok(Self::build(build_bytes, expression_work, expression_bytes))
    }
    fn build(limit: usize, work_limit: usize, expression_bytes_limit: usize) -> Self {
        Self {
            state: Arc::new(State {
                limit,
                used: AtomicUsize::new(0),
                work_limit,
                work: AtomicUsize::new(0),
                expression_bytes_limit,
                expression_bytes: AtomicUsize::new(0),
                failure: AtomicU8::new(ACTIVE),
            }),
        }
    }
    fn refusal(&self, actual: Option<usize>) -> ViewError {
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionBuildBytes,
            max: self.state.limit,
            actual,
        }
    }
    fn work_refusal(&self, actual: Option<usize>) -> ViewError {
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionWork,
            max: self.state.work_limit,
            actual,
        }
    }
    fn expression_bytes_refusal(&self, actual: Option<usize>) -> ViewError {
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionExpressionBytes,
            max: self.state.expression_bytes_limit,
            actual,
        }
    }
    /// Refuse publication if any participant exceeded the shared allowance, even if caught.
    pub fn check(&self) -> ViewResult<()> {
        match self.state.failure.load(Ordering::SeqCst) {
            ACTIVE => Ok(()),
            BYTE_LIMIT_REFUSAL => Err(self.refusal(None)),
            WORK_LIMIT_REFUSAL => Err(self.work_refusal(None)),
            EXPRESSION_BYTES_REFUSAL => Err(self.expression_bytes_refusal(None)),
            CONSTRUCTION_REFUSAL => Err(ViewError::InvalidDescription(
                "Description remains refused after an earlier construction failure".to_owned(),
            )),
            _ => Err(ViewError::InvariantViolation(
                "invalid description admission state",
            )),
        }
    }
    pub(crate) fn same_budget(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
    pub(crate) fn available(&self) -> usize {
        self.state.limit - self.state.used.load(Ordering::SeqCst)
    }
    pub(crate) fn poison(&self) {
        self.record_failure(CONSTRUCTION_REFUSAL);
    }
    fn record_failure(&self, reason: u8) {
        // Preserve the first failure class. A structural/serialization error is not evidence
        // that the byte ceiling was exceeded, and later failures must not rewrite its cause.
        let _previous =
            self.state
                .failure
                .compare_exchange(ACTIVE, reason, Ordering::SeqCst, Ordering::SeqCst);
    }
    fn add(&self, bytes: usize) -> ViewResult<()> {
        self.check()?;
        match self
            .state
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.state.limit)
            }) {
            Ok(_) => Ok(()),
            Err(used) => {
                self.record_failure(BYTE_LIMIT_REFUSAL);
                Err(self.refusal(used.checked_add(bytes)))
            }
        }
    }
    /// Charge expression work steps. The counter is cumulative for the request and sticky: a
    /// refusal names the first step past the limit and every later use of the request fails too.
    pub(crate) fn charge_work(&self, steps: usize) -> ViewResult<()> {
        self.check()?;
        match self
            .state
            .work
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_add(steps)
                    .filter(|total| *total <= self.state.work_limit)
            }) {
            Ok(_) => Ok(()),
            Err(used) => {
                self.record_failure(WORK_LIMIT_REFUSAL);
                Err(self.work_refusal(used.checked_add(steps)))
            }
        }
    }
    /// Charge aggregate encoded expression bytes, cumulative and sticky like the work steps.
    pub(crate) fn charge_expression_bytes(&self, bytes: usize) -> ViewResult<()> {
        self.check()?;
        match self
            .state
            .expression_bytes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.state.expression_bytes_limit)
            }) {
            Ok(_) => Ok(()),
            Err(used) => {
                self.record_failure(EXPRESSION_BYTES_REFUSAL);
                Err(self.expression_bytes_refusal(used.checked_add(bytes)))
            }
        }
    }
    /// Reserve bytes before materializing storage; release them only when that storage dies.
    pub fn reserve(&self, bytes: usize) -> ViewResult<DescriptionReservation> {
        self.add(bytes)?;
        let reservation = DescriptionReservation {
            state: Arc::clone(&self.state),
            bytes,
        };
        self.check()?;
        Ok(reservation)
    }
}
impl Default for DescriptionBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// Non-cloneable ownership of a portion of a construction allowance.
///
/// Retain this alongside the storage it admits. Dropping it releases only its own reservation;
/// an earlier failure remains sticky across all participants.
#[derive(Debug)]
pub struct DescriptionReservation {
    state: Arc<State>,
    bytes: usize,
}
impl DescriptionReservation {
    /// Release the unused part of a reservation once the storage it admitted has its final size.
    /// Release only: asking for more than is held is a bug in the caller's bound and fails
    /// loudly, poisoning the request instead of letting storage outlive its admission.
    pub(crate) fn shrink_to(&mut self, bytes: usize) -> ViewResult<()> {
        if bytes > self.bytes {
            DescriptionBudget {
                state: Arc::clone(&self.state),
            }
            .poison();
            return Err(ViewError::InvariantViolation(
                "description storage exceeds its reservation",
            ));
        }
        self.state
            .used
            .fetch_sub(self.bytes - bytes, Ordering::SeqCst);
        self.bytes = bytes;
        Ok(())
    }
    /// Admit additional retained bytes from this reservation's existing request allowance.
    ///
    /// Call before growing host-owned storage and retain this reservation alongside it. This
    /// never increases the request limit or creates a separate allowance. Dropping the
    /// reservation releases its admitted bytes but cannot clear an earlier refusal.
    ///
    /// # Errors
    /// Checked-size overflow, insufficient remaining bytes, or an earlier sticky refusal.
    /// A failed growth poisons the shared request allowance; no partial growth is admitted.
    ///
    /// ```
    /// let budget = erp_views::DescriptionBudget::with_limit(8)?;
    /// let mut reservation = budget.reserve(3)?;
    /// reservation.grow(5)?;
    /// budget.check()?;
    /// # Ok::<(), erp_views::ViewError>(())
    /// ```
    pub fn grow(&mut self, bytes: usize) -> ViewResult<()> {
        let budget = DescriptionBudget {
            state: Arc::clone(&self.state),
        };
        let Some(total) = self.bytes.checked_add(bytes) else {
            budget.record_failure(BYTE_LIMIT_REFUSAL);
            return Err(budget.refusal(None));
        };
        budget.add(bytes)?;
        self.bytes = total;
        budget.check()
    }
}
impl Drop for DescriptionReservation {
    fn drop(&mut self) {
        self.state.used.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

fn allocation_refusal(budget: &DescriptionBudget, message: &str) -> ViewError {
    budget.poison();
    ViewError::InvalidDescription(message.to_owned())
}
pub(crate) fn storage<T>(
    budget: &DescriptionBudget,
    count: usize,
) -> ViewResult<(Vec<T>, DescriptionReservation)> {
    let bytes = count
        .checked_mul(size_of::<T>())
        .ok_or_else(|| allocation_refusal(budget, "Description collection storage overflow"))?;
    let mut reservation = budget.reserve(bytes)?;
    let mut values = Vec::new();
    if values.try_reserve_exact(count).is_err() {
        return Err(allocation_refusal(
            budget,
            "Description collection allocation failed",
        ));
    }
    if values.capacity() > count {
        let extra = (values.capacity() - count)
            .checked_mul(size_of::<T>())
            .ok_or_else(|| allocation_refusal(budget, "Description collection storage overflow"))?;
        reservation.grow(extra)?;
    }
    Ok((values, reservation))
}
