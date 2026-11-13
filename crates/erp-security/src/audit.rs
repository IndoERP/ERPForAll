// SPDX-License-Identifier: MIT OR Apache-2.0
//! Audit capability registration. As with job capabilities, the registration
//! lives above erp-orm, whose unsafe-code boundary excludes linker-section contributions.
//! The reader resolves this exact registered capability, never a caller-selected capability.
erp_orm::capability!(
    /// Read eligible audit summaries, also requiring current audit-model and target permissions.
    /// Base maps this capability once in `data/ir.capability.csv`.
    pub AUDIT_READ = "audit.read"
);
