// SPDX-License-Identifier: MIT OR Apache-2.0
//! Format-1 hashing, owner-only chain metadata and external witness contracts.
//!
//! Codec callers supply already redacted persisted values. A digest alone is not evidence of an
//! external witness. Explicit owner operations install chain metadata and create linked epochs.
//! `AuditWitness` supports publication and scoped fetch; the file implementation lives in erp-cli.
//! `Transaction::seal_audit_batch` stages a safe-prefix seal in its owner transaction. Confirm
//! commit before publishing an anchor. `Transaction::verify_audit_chain` re-reads rows, seals and epoch
//! links in one snapshot and compares them with an independently read anchor inventory; the CLI
//! command and recovery remain separate packages.
//!
//! ```
//! use erp_orm::audit::seal::CanonicalValue;
//! let exact = CanonicalValue::numeric("9007199254740992.100")?;
//! assert_eq!(exact, CanonicalValue::numeric("9007199254740992.1")?);
//! # Ok::<(), erp_orm::audit::seal::CodecError>(())
//! ```
mod epoch;
mod grammar;
mod hashes;
mod json;
mod number;
mod problem;
mod row;
mod schema;
mod sealer;
mod verify;
mod witness;

pub use epoch::{EpochChange, EpochReason, EpochState};
pub use grammar::{CanonicalValue, CodecError};
pub use hashes::{BatchDigest, RowDigest, SealCursor, SealIdentity, SealInput};
pub use schema::{ChainProblem, ChainSchemaReport};
pub use witness::{Anchor, AnchorRange, AuditWitness, WitnessError, WitnessOperation};

pub use problem::SealerProblem;
pub use sealer::{SealOptions, SealOutcome, SealReport};
pub use verify::{
    AnchorInventory, Assurance, FailurePoint, TailSummary, VerifyFailure, VerifyOutcome,
    VerifyProblem, VerifyRefusal, VerifyReport, VerifyScope, WitnessEvidence,
};
