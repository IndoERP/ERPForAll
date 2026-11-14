// SPDX-License-Identifier: MIT OR Apache-2.0
//! Nonsecret effective upload limits shared by bootstrap and live client preflight.
use erp_sdk::{Result, Transaction};

/// Validated effective limits. The inline limit may be zero: only an empty file fits that path.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct UploadPolicy {
    max: u64,
    inline: u64,
}
impl UploadPolicy {
    /// Maximum file size after applying the database policy and the node's existing ceiling.
    pub fn max_file_bytes(&self) -> u64 {
        self.max
    }
    /// Maximum decoded bytes accepted by the current inline upload path.
    pub fn inline_max_bytes(&self) -> u64 {
        self.inline
    }
}

/// Read current database policy and the runtime of this transaction's registry generation.
/// No process cache is used; malformed settings or absent attachment runtime return an error.
/// This internal policy read does not grant access to generic system parameters.
///
/// # Errors
/// `Validation` for a malformed database policy; `User` when the runtime is not configured;
/// database/flush errors retain their original kind.
///
/// ```no_run
/// # async fn example(tx: &mut erp_sdk::Transaction) -> erp_sdk::Result<()> {
/// let policy = erp_addon_base::attachments::upload_policy(tx).await?;
/// assert!(policy.inline_max_bytes() <= policy.max_file_bytes());
/// # Ok(())
/// # }
/// ```
pub async fn upload_policy(tx: &mut Transaction) -> Result<UploadPolicy> {
    let runtime = super::runtime::runtime(&tx.env())?;
    let max = super::settings::max_file_bytes(tx)
        .await?
        .min(runtime.limits().max_file_bytes);
    Ok(UploadPolicy {
        max,
        inline: max.min(runtime.limits().inline_max_bytes as u64),
    })
}
