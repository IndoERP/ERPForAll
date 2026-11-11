//! The groups of a field stored in the database (`ir.model.fields.groups`, relation
//! `ir_model_fields_group_rel`): the overlay of
//! field groups for custom `x_` fields that the access engine reads with the policy (
//! design note §2.3). The registry's own fields keep their `groups` in the declaration.
//!
//! Both tables bump the `policy` generation (Revision 3): the relation on
//! every change, `ir_model_fields` on insert, delete and truncate only, so renaming a field or
//! changing its help text (a module upgrade updates those columns) leaves the policy alone.

use erp_sdk::prelude::*;

use crate::IrModelFields;

/// `ir.model.fields` + `groups`.
#[extend(
    IrModelFields,
    signal_triggers = [
        ("_policy_bump", "policy", update = false),
        ("_policy_bump_groups", "policy", relation = "groups"),
    ],
)]
pub struct IrModelFieldsGroups {
    /// Groups that may access the field (empty: everyone).
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "ir_model_fields_group_rel",
        column1 = "field_id",
        column2 = "group_id"
    )]
    pub groups: Vec<RecId>,
}
