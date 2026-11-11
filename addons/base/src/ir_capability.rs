//! `ir.capability` (table `ir_capability`): which groups hold a named capability (
//! decision 7, Revision 3 ruling c; design note §2.3).
//! The policy snapshot reads the rows; a
//! registered capability without a row is an error of the snapshot build, never a silent
//! `false`.
//!
//! `base` ships the rows of the core capabilities in `data/ir_capability.csv` (loaded by the
//! data loader). Every write bumps the `policy` generation (statement-level trigger).

use erp_sdk::prelude::*;

/// `ir.capability`: one row per capability name.
#[model(
    name = "ir.capability",
    description = "Capability",
    order = "name",
    unique_indexes = [(
        "_name_uniq",
        "(name)",
        "A capability is mapped to groups once."
    )],
    signal_triggers = [("_policy_bump", "policy")],
)]
pub struct IrCapability {
    /// The capability's name, as declared with `capability!` (`principal.impersonate`).
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// The group spec that holds it (`base.group_system`).
    #[field(char, required = true)]
    pub groups: Option<Arc<str>>,
    /// What it allows, for administrators.
    #[field(text)]
    pub description: Option<Arc<str>>,
}
