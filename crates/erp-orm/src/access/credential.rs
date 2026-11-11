//! The credential variant of the principal resolver:
//! a credential presented with a request (a session secret today, an API key later) is checked
//! and the principal resolved **in the transaction's opening flight**, one statement next to
//! `BEGIN`, the limits and the signal read, on the primary.
//!
//! The ORM does not know sessions: the caller (`erp-security`) supplies a [`CredentialCheck`],
//! whose statement reads the credential row joined to the subject's resolution, and whose
//! [`CredentialCheck::finish`] decides from the rows. Until `finish` accepts, the transaction holds
//! an unresolved placeholder principal with no elevation and no groups, whose subject names no
//! row; it is never handed out. A refusal rolls the transaction back.

use erp_fields::Result;
use tokio_postgres::Row;

use super::{AccessPolicy, ResolverStatement};
use crate::principal::Principal;
use crate::transaction::Transaction;

/// A credential checked in the opening flight.
pub trait CredentialCheck: Send + Sync {
    /// What an accepted check hands back with the transaction (the checked session).
    type Accepted: Send;
    /// Why a check refused (and what the caller should clean up).
    type Refused: Send;

    /// The statement: reads the credential row and the subject's resolution columns in one row.
    ///
    /// # Errors
    ///
    /// The check's own (an input it cannot express).
    fn statement(&self) -> Result<ResolverStatement>;

    /// The verdict from the statement's rows, the resolved principal's groups closed under
    /// `policy` (the snapshot the transaction pinned after its signal read).
    ///
    /// # Errors
    ///
    /// Undecodable rows, a group the policy does not know: the transaction is rolled back and the
    /// error returned (a refusal is a verdict, not an error).
    fn finish(
        &self,
        rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> Result<CredentialVerdict<Self::Accepted, Self::Refused>>;
}

/// What [`CredentialCheck::finish`] decides.
#[derive(Debug)]
#[non_exhaustive]
pub enum CredentialVerdict<A, R> {
    /// The credential is valid; the transaction acts as `principal`.
    Accepted {
        /// The resolved principal (kind, subject, groups, companies).
        principal: Principal,
        /// What the caller gets back.
        value: A,
    },
    /// The credential is refused; the transaction is rolled back.
    Refused(R),
}

impl<A, R> CredentialVerdict<A, R> {
    /// An accepted verdict.
    pub fn accepted(principal: Principal, value: A) -> Self {
        Self::Accepted { principal, value }
    }
}

/// What [`crate::Orm::begin_credential`] gives.
#[allow(
    clippy::large_enum_variant,
    reason = "returned once per request and destructured at once; boxing the transaction would \
              add an allocation to every authenticated request"
)]
pub enum CredentialBegin<A, R> {
    /// The transaction, open on the primary as the credential's principal, with the check's value.
    Open {
        /// The transaction.
        tx: Transaction,
        /// The check's value.
        value: A,
    },
    /// The check refused; nothing is open.
    Refused(R),
}

impl<A: std::fmt::Debug, R: std::fmt::Debug> std::fmt::Debug for CredentialBegin<A, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open { value, .. } => f
                .debug_struct("Open")
                .field("value", value)
                .finish_non_exhaustive(),
            Self::Refused(reason) => f.debug_tuple("Refused").field(reason).finish(),
        }
    }
}
