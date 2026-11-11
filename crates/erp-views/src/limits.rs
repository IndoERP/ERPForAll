//! Resource limits (engineering standards §4): on by default, a host can only lower
//! them, zero is never "unlimited".

use crate::error::{LimitKind, ViewError, ViewResult};

/// Limits applied to parsing, XPath evaluation and combination.
///
/// ```
/// use erp_views::{LimitKind, ViewLimits};
///
/// let limits = ViewLimits::default().lowered(LimitKind::Depth, 64)?;
/// assert_eq!(limits.get(LimitKind::Depth), 64);
/// assert!(ViewLimits::default().lowered(LimitKind::Depth, 0).is_err());
/// # Ok::<(), erp_views::ViewError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ViewLimits {
    description_encoded_bytes: usize,
    description_build_bytes: usize,
    description_expression_work: usize,
    description_expression_bytes: usize,
    arch_bytes: usize,
    arch_elements: usize,
    arch_nodes: usize,
    element_attributes: usize,
    depth: usize,
    combined_nodes: usize,
    combined_bytes: usize,
    xpath_length: usize,
    xpath_nesting: usize,
    xpath_steps: usize,
    evaluation_steps: usize,
    inheritance_depth: usize,
    catalogue_views: usize,
    catalogue_bytes: usize,
    check_group_refs: usize,
    batch_steps: usize,
    batch_bytes: usize,
}

impl ViewLimits {
    /// The defaults, which are also the maximum values.
    pub const DEFAULT: ViewLimits = ViewLimits {
        description_encoded_bytes: 8 * 1024 * 1024,
        description_build_bytes: 32 * 1024 * 1024,
        description_expression_work: 1_000_000,
        description_expression_bytes: 16 * 1024 * 1024,
        arch_bytes: 10 * 1024 * 1024,
        arch_elements: 10_000,
        arch_nodes: 40_000,
        element_attributes: 256,
        depth: 256,
        combined_nodes: 200_000,
        combined_bytes: 32 * 1024 * 1024,
        xpath_length: 4096,
        xpath_nesting: 32,
        xpath_steps: 64,
        evaluation_steps: 20_000_000,
        inheritance_depth: 128,
        catalogue_views: 10_000,
        catalogue_bytes: 64 * 1024 * 1024,
        check_group_refs: 4_096,
        batch_steps: 100_000_000,
        batch_bytes: 128 * 1024 * 1024,
    };

    /// The current value of one limit.
    #[must_use]
    pub const fn get(&self, kind: LimitKind) -> usize {
        match kind {
            LimitKind::DescriptionEncodedBytes => self.description_encoded_bytes,
            LimitKind::DescriptionBuildBytes => self.description_build_bytes,
            LimitKind::DescriptionExpressionWork => self.description_expression_work,
            LimitKind::DescriptionExpressionBytes => self.description_expression_bytes,
            LimitKind::ArchBytes => self.arch_bytes,
            LimitKind::ArchElements => self.arch_elements,
            LimitKind::ArchNodes => self.arch_nodes,
            LimitKind::ElementAttributes => self.element_attributes,
            LimitKind::Depth => self.depth,
            LimitKind::CombinedNodes => self.combined_nodes,
            LimitKind::CombinedBytes => self.combined_bytes,
            LimitKind::XpathLength => self.xpath_length,
            LimitKind::XpathNesting => self.xpath_nesting,
            LimitKind::XpathSteps => self.xpath_steps,
            LimitKind::EvaluationSteps => self.evaluation_steps,
            LimitKind::InheritanceDepth => self.inheritance_depth,
            LimitKind::CatalogueViews => self.catalogue_views,
            LimitKind::CatalogueBytes => self.catalogue_bytes,
            LimitKind::CheckGroupRefs => self.check_group_refs,
            LimitKind::BatchSteps => self.batch_steps,
            LimitKind::BatchBytes => self.batch_bytes,
        }
    }

    /// Returns a copy with one limit lowered to `value`.
    ///
    /// # Errors
    ///
    /// [`ViewError::InvalidLimit`] when `value` is zero or above the default.
    pub fn lowered(mut self, kind: LimitKind, value: usize) -> ViewResult<Self> {
        let max = Self::DEFAULT.get(kind);
        if value == 0 || value > max {
            return Err(ViewError::InvalidLimit {
                limit: kind,
                value,
                max,
            });
        }
        let slot = match kind {
            LimitKind::DescriptionEncodedBytes => &mut self.description_encoded_bytes,
            LimitKind::DescriptionBuildBytes => &mut self.description_build_bytes,
            LimitKind::DescriptionExpressionWork => &mut self.description_expression_work,
            LimitKind::DescriptionExpressionBytes => &mut self.description_expression_bytes,
            LimitKind::ArchBytes => &mut self.arch_bytes,
            LimitKind::ArchElements => &mut self.arch_elements,
            LimitKind::ArchNodes => &mut self.arch_nodes,
            LimitKind::ElementAttributes => &mut self.element_attributes,
            LimitKind::Depth => &mut self.depth,
            LimitKind::CombinedNodes => &mut self.combined_nodes,
            LimitKind::CombinedBytes => &mut self.combined_bytes,
            LimitKind::XpathLength => &mut self.xpath_length,
            LimitKind::XpathNesting => &mut self.xpath_nesting,
            LimitKind::XpathSteps => &mut self.xpath_steps,
            LimitKind::EvaluationSteps => &mut self.evaluation_steps,
            LimitKind::InheritanceDepth => &mut self.inheritance_depth,
            LimitKind::CatalogueViews => &mut self.catalogue_views,
            LimitKind::CatalogueBytes => &mut self.catalogue_bytes,
            LimitKind::CheckGroupRefs => &mut self.check_group_refs,
            LimitKind::BatchSteps => &mut self.batch_steps,
            LimitKind::BatchBytes => &mut self.batch_bytes,
        };
        *slot = value;
        Ok(self)
    }

    /// Error value for `kind` with an optional reached value.
    pub(crate) fn exceeded(&self, kind: LimitKind, actual: Option<usize>) -> ViewError {
        ViewError::LimitExceeded {
            limit: kind,
            max: self.get(kind),
            actual,
        }
    }
}

impl Default for ViewLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Bytes of string work (compare, split, search, copy) that cost one evaluation step.
pub(crate) const BYTES_PER_STEP: usize = 16;

/// The evaluation-step counter of one combination ([`LimitKind::EvaluationSteps`]), optionally
/// drawing from what is left of a batch ([`LimitKind::BatchSteps`], [`Budget::within_batch`]).
///
/// Work whose amount grows with specs × nodes, attributes, children or bytes is charged before it
/// is done, in units: one per node visited, attribute scanned, child visited, token handled or
/// mark-table slot group, plus one per [`BYTES_PER_STEP`] bytes compared, split or searched. A
/// charge of `n` steps is `n` unit charges, so the error always names the first step past the
/// limit (`max + 1`) and no work happens past it.
#[derive(Debug)]
pub(crate) struct Budget {
    spent: usize,
    max: usize,
    /// The limit the error names when `max` is reached: `EvaluationSteps` with `max`, or
    /// `BatchSteps` with the batch's maximum.
    reached: Reached,
}

#[derive(Debug, Clone, Copy)]
enum Reached {
    Combination,
    Batch { max: usize },
}

impl Budget {
    pub(crate) fn new(limits: &ViewLimits) -> Self {
        Self {
            spent: 0,
            max: limits.get(LimitKind::EvaluationSteps),
            reached: Reached::Combination,
        }
    }

    /// The budget of one combination of a batch that already spent `before` steps: the
    /// combination's own limit, or what is left of the batch's when that is less.
    pub(crate) fn within_batch(limits: &ViewLimits, before: usize) -> Self {
        let own = limits.get(LimitKind::EvaluationSteps);
        let batch = limits.get(LimitKind::BatchSteps);
        let left = batch.saturating_sub(before);
        if left < own {
            Self {
                spent: 0,
                max: left,
                reached: Reached::Batch { max: batch },
            }
        } else {
            Self {
                spent: 0,
                max: own,
                reached: Reached::Combination,
            }
        }
    }

    /// Charges `n` steps.
    ///
    /// # Errors
    ///
    /// [`ViewError::LimitExceeded`] for [`LimitKind::EvaluationSteps`] (or
    /// [`LimitKind::BatchSteps`] when the batch's limit is the nearer one) when the steps would
    /// cross the limit; the counter is then pinned at the limit, so every later charge fails too.
    pub(crate) fn charge_n(&mut self, n: usize) -> ViewResult<()> {
        match self.spent.checked_add(n) {
            Some(next) if next <= self.max => {
                self.spent = next;
                Ok(())
            }
            _ => {
                self.spent = self.max;
                Err(match self.reached {
                    Reached::Combination => ViewError::LimitExceeded {
                        limit: LimitKind::EvaluationSteps,
                        max: self.max,
                        actual: self.max.checked_add(1),
                    },
                    Reached::Batch { max } => ViewError::LimitExceeded {
                        limit: LimitKind::BatchSteps,
                        max,
                        actual: max.checked_add(1),
                    },
                })
            }
        }
    }

    /// Charges one step.
    ///
    /// # Errors
    ///
    /// As [`Budget::charge_n`].
    pub(crate) fn charge(&mut self) -> ViewResult<()> {
        self.charge_n(1)
    }

    /// Charges one step plus one per [`BYTES_PER_STEP`] bytes of `len` bytes of string work.
    ///
    /// # Errors
    ///
    /// As [`Budget::charge_n`].
    pub(crate) fn charge_bytes(&mut self, len: usize) -> ViewResult<()> {
        self.charge_n(1 + len / BYTES_PER_STEP)
    }

    /// `a == b`, charged: one step when the lengths differ (no byte is compared), otherwise the
    /// bytes compared.
    ///
    /// # Errors
    ///
    /// As [`Budget::charge_n`].
    pub(crate) fn str_eq(&mut self, a: &str, b: &str) -> ViewResult<bool> {
        if a.len() != b.len() {
            self.charge()?;
            return Ok(false);
        }
        self.charge_bytes(a.len())?;
        Ok(a == b)
    }

    /// A table of one flag per arena slot (live, removed and sentinel nodes), charged by its size:
    /// a combination that removed many nodes still pays for the slots it zeroes.
    ///
    /// # Errors
    ///
    /// As [`Budget::charge_n`].
    pub(crate) fn slot_table(&mut self, slots: usize) -> ViewResult<Vec<bool>> {
        self.charge_bytes(slots)?;
        Ok(vec![false; slots])
    }

    /// Steps charged so far.
    pub(crate) fn spent(&self) -> usize {
        self.spent
    }
}
