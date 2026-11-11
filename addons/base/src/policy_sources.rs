//! The base models whose ORM writes change the access policy or a principal's membership
//! , registered on the engine's
//! `PolicySourceWrite` point: a write flushes pending computes first and marks the transaction,
//! and its next engine call flushes these models' pending writes before it rebuilds the policy
//! privately (policy tables) or re-reads the bound principals (membership).
//!
//! The database triggers bump the `policy` generation for other processes; these registrations
//! keep the writing transaction itself current (SEC-018).

use erp_sdk::access::{PolicySourceWrite, SourceKind, policy_source};

policy_source!(
    /// Permissions and restrictions.
    IR_ACCESS = PolicySourceWrite::new("ir.access", SourceKind::Policy)
);

policy_source!(
    /// Groups, their implications and their names (messages list them); writing `user_ids`
    /// changes membership too, which a policy change re-reads anyway.
    RES_GROUPS = PolicySourceWrite::new("res.groups", SourceKind::Policy)
);

policy_source!(
    /// Privileges (the group graph's scopes).
    RES_GROUPS_PRIVILEGE = PolicySourceWrite::new("res.groups.privilege", SourceKind::Policy)
);

policy_source!(
    /// Capability mappings.
    IR_CAPABILITY = PolicySourceWrite::new("ir.capability", SourceKind::Policy)
);

policy_source!(
    /// The groups of custom fields; creating or deleting a field record counts as well.
    IR_MODEL_FIELDS =
        PolicySourceWrite::new("ir.model.fields", SourceKind::Policy).fields(&["groups"])
);

policy_source!(
    /// Deleting a model deletes its rules and fields (foreign-key cascade); its other writes do
    /// not touch the policy.
    IR_MODEL = PolicySourceWrite::new("ir.model", SourceKind::Policy).fields(&[])
);

policy_source!(
    /// A user's groups, companies, default company and archiving.
    RES_USERS = PolicySourceWrite::new("res.users", SourceKind::Membership).fields(&[
        "group_ids",
        "company_ids",
        "company_id",
        "active"
    ])
);

policy_source!(
    /// Company archiving changes allowed companies; deletion removes memberships.
    RES_COMPANY = PolicySourceWrite::new("res.company", SourceKind::Membership).fields(&["active"])
);
