// SPDX-License-Identifier: MIT OR Apache-2.0
//! The capability behind `with_user` to another user (A4, slice P5). As
//! with the job and audit capabilities, the registration lives above `erp-orm`, whose unsafe-code
//! boundary excludes linker-section contributions; the ORM is told which capability it is with
//! `OrmBuilder::impersonate_capability(&erp_security::principal::PRINCIPAL_IMPERSONATE)`.

erp_orm::capability!(
    /// Act as another user with `with_user` from a principal that is not `System` (the engine
    /// asks it of the switching principal's own subject; a scope never carries it). Base maps it
    /// to `base.group_system` in `data/ir.capability.csv`.
    pub PRINCIPAL_IMPERSONATE = "principal.impersonate"
);
