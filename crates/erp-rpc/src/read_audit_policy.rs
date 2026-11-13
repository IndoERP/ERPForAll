// SPDX-License-Identifier: MIT OR Apache-2.0
//! Validated additive read-audit selection.
use erp_fields::{Error, Result};
use erp_orm::QualifiedXmlId;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Selection {
    #[default]
    Defaults,
    AllUsers,
    Groups(Box<[QualifiedXmlId]>),
}

/// Operator widening of successful-read auditing. It cannot disable mandatory selections.
/// Group names are canonical, positive XML IDs; membership is resolved per transaction.
///
/// ```
/// use erp_rpc::ReadAuditPolicy;
/// let policy = ReadAuditPolicy::user_groups(&["base.group_system"])?;
/// assert_ne!(policy, ReadAuditPolicy::default());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ReadAuditPolicy {
    selection: Selection,
}

impl ReadAuditPolicy {
    /// Additionally select every User read. Model opt-in, elevation and mandatory attempts stay on.
    pub fn all_users() -> Self {
        Self {
            selection: Selection::AllUsers,
        }
    }

    /// Additionally select users in any of these positive group XML IDs.
    /// Empty means defaults-only; order and duplicates do not affect the canonical policy.
    ///
    /// # Errors
    /// `Programming` for more than 256 supplied entries or an invalid ID/expression.
    pub fn user_groups(names: &[&str]) -> Result<Self> {
        if names.len() > 256 {
            return Err(Error::programming(
                "audit_read_policy.user_groups has more than 256 entries",
            ));
        }
        let invalid = || {
            Error::programming(
                "audit_read_policy.user_groups must contain positive qualified group XML IDs without expression syntax (maximum 256 UTF-8 bytes each)",
            )
        };
        let mut groups = Vec::with_capacity(names.len());
        for name in names {
            if name.contains([',', '!', '|', '&', '(', ')']) {
                return Err(invalid());
            }
            groups.push(QualifiedXmlId::parse(name).map_err(|_| invalid())?);
        }
        groups.sort_unstable();
        groups.dedup();
        Ok(Self {
            selection: if groups.is_empty() {
                Selection::Defaults
            } else {
                Selection::Groups(groups.into_boxed_slice())
            },
        })
    }

    /// Evaluate only the additive user selection, using the original transaction binding.
    /// Resolve configured groups even when another mandatory selection already applies.
    pub(crate) async fn selects_user(&self, tx: &mut erp_orm::Transaction) -> Result<bool> {
        let env = tx.env();
        let selected = match &self.selection {
            Selection::Defaults => false,
            Selection::AllUsers => true,
            Selection::Groups(groups) => tx
                .group_membership(&env)
                .await?
                .has_any_effective_xmlid(groups)?,
        };
        Ok(selected && env.principal().kind() == erp_orm::PrincipalKind::User)
    }

    pub(crate) fn describe(
        &self,
        out: &mut erp_orm::node::SettingsWriter<'_>,
    ) -> std::result::Result<(), erp_orm::node::NodeError> {
        let (mode, groups) = match &self.selection {
            Selection::Defaults => ("defaults", String::new()),
            Selection::AllUsers => ("all_users", String::new()),
            Selection::Groups(ids) => {
                // A maximum policy plus separators exceeds a shared string's 64 KiB bound.
                // Hash the sorted IDs with a NUL delimiter (forbidden inside qualified IDs).
                let mut hash = Sha256::new();
                hash.update(b"erpforall/read-audit-groups/v1\0");
                for id in ids {
                    hash.update(id.as_str().as_bytes());
                    hash.update([0]);
                }
                (
                    "groups",
                    erp_orm::node::Digest::from_bytes(hash.finalize().into()).to_string(),
                )
            }
        };
        out.shared_str("rpc.audit_read_policy.mode", mode)?;
        out.shared_str("rpc.audit_read_policy.groups_digest", &groups)
    }
}
