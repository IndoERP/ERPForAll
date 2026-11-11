//! Operation-local address synchronization admission accounting.

use erp_sdk::{Error, Result};

/// Counts scheduled field assignments across all reentrant work in one operation.
#[derive(Default)]
pub(crate) struct AssignmentBudget {
    used: usize,
}

impl AssignmentBudget {
    /// Charge before scheduling assignments; a failed charge changes no accounting.
    pub(crate) fn charge(&mut self, assignments: usize) -> Result<()> {
        let next = self.used.checked_add(assignments).ok_or_else(exceeded)?;
        if next > 65_536 {
            return Err(exceeded());
        }
        self.used = next;
        Ok(())
    }
}

fn exceeded() -> Error {
    Error::validation(
        "Partner address synchronization maximum field assignments of 65536 exceeded.",
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "admission boundary tests")]
mod tests {
    use super::AssignmentBudget;
    use erp_sdk::ErrorKind;

    #[test]
    fn assignment_budget_admits_exact_limit_then_refuses_next_assignment() {
        let mut budget = AssignmentBudget::default();
        budget.charge(65_535).unwrap();
        budget.charge(1).unwrap();
        let error = budget.charge(1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Partner address synchronization maximum field assignments of 65536 exceeded."
        );
    }

    #[test]
    fn assignment_budget_refuses_overflow_without_wrapping() {
        let mut budget = AssignmentBudget::default();
        budget.charge(1).unwrap();
        let error = budget.charge(usize::MAX).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Partner address synchronization maximum field assignments of 65536 exceeded."
        );
    }
}
