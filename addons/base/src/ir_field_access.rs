// SPDX-License-Identifier: MIT OR Apache-2.0
//! `ir.field.access` (table `ir_field_access`): an administrator's override of the groups of one
//! field, declared or custom.
//!
//! A field's `groups` are written in its declaration, so changing who may read and write it meant
//! changing the addon. An override changes it from the user interface instead:
//!
//! - `extend` ("Also require"): its groups are required as well as the declared ones (a user
//!   must be in one of the declared groups **and** in one of these). It only narrows.
//! - `replace`: its groups take the place of the declared ones; no group opens the field to
//!   everyone who may access the model. The groups of a custom field (`ir.model.fields`) stay.
//!
//! The access engine reads the overrides with the rest of the policy (`erp-security`'s loader), so
//! they apply wherever declared groups do: reads, writes, searches, `fields_get` and the fields a
//! view description shows. One field has at most one override. A field declared for nobody
//! (`groups = "."`: key digests, encrypted material) cannot be overridden: the constraint below
//! refuses it, and the engine ignores a `replace` row for such a field however it was written.
//!
//! Every write bumps the `policy` generation (statement-level triggers on the table and on its
//! group relation) and, like every mutating call, leaves an audit row. The compiled views are
//! recaptured, because a `replace` override changes the guard a view node takes from its field.

use erp_sdk::prelude::*;

use crate::ir_ui_view::TEMPLATES;

/// The groups text of a field declared for nobody.
const NEVER: &str = ".";

/// `ir.field.access`: one override of one field's groups.
#[model(
    name = "ir.field.access",
    description = "Field Access",
    order = "field_id",
    unique_indexes = [(
        "_field_uniq",
        "(field_id)",
        "A field has one access override; change the existing one."
    )],
    signal_triggers = [
        ("_policy_bump", "policy"),
        ("_policy_bump_groups", "policy", relation = "group_ids"),
    ],
)]
pub struct IrFieldAccess {
    /// The field whose groups are overridden.
    #[field(
        many2one,
        comodel = "ir.model.fields",
        string = "Field",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub field_id: Option<RecId>,
    /// `extend`: also require these groups; `replace`: these groups instead of the declared ones.
    #[field(
        selection,
        selection = [
            ("extend", "Also require these groups"),
            ("replace", "Replace the declared groups"),
        ],
        default = "extend",
        required = true
    )]
    pub mode: Option<Arc<str>>,
    /// The groups (any one of them is enough).
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "ir_field_access_group_rel",
        column1 = "access_id",
        column2 = "group_id",
        string = "Groups"
    )]
    pub group_ids: Vec<RecId>,
    /// Why the override exists, for the next administrator.
    #[field(text)]
    pub note: Option<Arc<str>>,
}

#[methods(IrFieldAccess, addon = "base")]
impl IrFieldAccess {
    /// A field declared for nobody keeps its declaration; an `extend` override needs a group
    /// (without one it would change nothing).
    #[constrains("field_id", "mode", "group_ids")]
    async fn _check_field_access(
        this: &RecordSet<IrFieldAccess>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let env = this.as_records().env().clone();
        for rec in this.iter() {
            let field = rec
                .get(tx, IrFieldAccess::FIELD_ID)
                .await?
                .ok_or_else(|| Error::validation("A field access override names a field."))?;
            let (model, name) = field_name(&env, tx, field).await?;
            let spec = env
                .registry()
                .model(&model)
                .and_then(|spec| spec.field(&name))
                .ok_or_else(|| {
                    Error::validation(format!(
                        "The field '{name}' of {model} is not loaded; it cannot get an access \
                         override."
                    ))
                })?;
            if spec.groups.is_some_and(|text| text.trim() == NEVER) {
                return Err(Error::validation(format!(
                    "The field '{name}' of {model} is closed to every user (it holds secret \
                     material); its groups cannot be overridden."
                )));
            }
            let mode = rec.get(tx, IrFieldAccess::MODE).await?;
            let groups = rec.get(tx, IrFieldAccess::GROUP_IDS).await?;
            if mode.as_deref() == Some("extend") && groups.is_empty() {
                return Err(Error::validation(
                    "An override that also requires groups needs at least one group.",
                ));
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<IrFieldAccess>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let created = sup.call(tx, this.as_records(), (vals_list,)).await?;
        tx.invalidate_ormcache(TEMPLATES);
        Ok(created)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<IrFieldAccess>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), (vals,)).await?;
        tx.invalidate_ormcache(TEMPLATES);
        Ok(())
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<IrFieldAccess>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        tx.invalidate_ormcache(TEMPLATES);
        Ok(())
    }
}

/// The model and name of the `ir.model.fields` record `field`.
async fn field_name(env: &Env, tx: &mut Transaction, field: RecId) -> Result<(String, String)> {
    let rows = env
        .records("ir.model.fields")?
        .browse([field.get()])?
        .read(tx, &["model", "name"])
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::validation("The field of an access override does not exist."))?;
    let text = |key: &str| match row.get(key) {
        Some(Value::Str(value)) => Ok(value.to_string()),
        _ => Err(Error::invariant(format!(
            "ir.model.fields {} has no {key}",
            field.get()
        ))),
    };
    Ok((text("model")?, text("name")?))
}
