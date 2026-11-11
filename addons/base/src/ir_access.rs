//! `ir.access`: model and record access in one table (SPEC-SEC §4.1,
//! ;). A row with a group is a
//! **permission** of that group, a row without one a **restriction** for everyone; the access
//! domain of a model and operation is `OR(permissions of the user's groups) AND
//! AND(restrictions)`. `erp-security` compiles the active rows into the policy snapshot.
//!
//! Every write (ORM, raw SQL, the cascade of a deleted model or group) bumps the `policy`
//! generation through a statement-level trigger.
//!
//! Not yet here: the computed `kind`, `is_standard`, `for_read`/`for_write`/`for_create`/
//! `for_unlink` (with their inverse) and `note` of the access screen (M4), domain validation at
//! write time.

use erp_sdk::prelude::*;

/// `ir.access`: one permission or restriction.
#[model(
    name = "ir.access",
    description = "Access",
    order = "model_id, group_id, id",
    unique_indexes = [(
        "_generated_model_uniq",
        "(model_id) WHERE generated",
        "A model has one generated company restriction."
    )],
    signal_triggers = [("_policy_bump", "policy")],
)]
pub struct IrAccess {
    /// Label.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Inactive rows are ignored by the policy.
    #[field(boolean, default = true)]
    pub active: bool,
    /// The model it applies to.
    #[field(
        many2one,
        comodel = "ir.model",
        string = "Model",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub model_id: Option<RecId>,
    /// The group of a permission; empty for a restriction.
    #[field(
        many2one,
        comodel = "res.groups",
        string = "Group",
        ondelete = "cascade",
        index = true
    )]
    pub group_id: Option<RecId>,
    /// The operations it covers: a non-empty subset of `crud`, in `c, r, u, d` order.
    #[field(
        selection,
        selection = [
            ("crud", "Create, Read, Update, Delete"),
            ("cru", "Create, Read, Update"),
            ("crd", "Create, Read, Delete"),
            ("cud", "Create, Update, Delete"),
            ("rud", "Read, Update, Delete"),
            ("cr", "Create, Read"),
            ("cu", "Create, Update"),
            ("cd", "Create, Delete"),
            ("ru", "Read, Update"),
            ("rd", "Read, Delete"),
            ("ud", "Update, Delete"),
            ("c", "Create"),
            ("r", "Read"),
            ("u", "Update"),
            ("d", "Delete"),
        ],
        required = true
    )]
    pub operation: Option<Arc<str>>,
    /// The records the operations are allowed on (a domain); empty for every record.
    #[field(char)]
    pub domain: Option<Arc<str>>,
    /// The row is the company restriction generated from a model's `company_access`
    /// declaration (A1; design).
    /// Module installation and updates write it; nothing
    /// else may through the ORM. Not `required` at the
    /// database: the schema layer drops column defaults, and rows inserted with plain SQL
    /// (tools, tests) must keep working; the loader reads `NULL` as false.
    #[field(boolean, default = false, readonly = true, copy = false)]
    pub generated: bool,
}

const GENERATED_INPUT_ERROR: &str = "ir.access rows marked generated are written by module installation and updates only: the company restriction of a model comes from its company_access declaration.";

#[methods(IrAccess, addon = "base")]
impl IrAccess {
    /// Only module operations may supply the generated marker, including explicit false.
    #[method(override)]
    async fn create(
        this: &RecordSet<IrAccess>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if vals_list.iter().any(has_generated) {
            return Err(Error::user(GENERATED_INPUT_ERROR));
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// Defaults are resolved inside the create super call; they must not manufacture a marker.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<IrAccess>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let values: erp_sdk::ValueMap = sup.call(tx, this.as_records(), (names,)).await?;
        if let Some(value) = values.get("generated") {
            match value {
                Value::Bool(false) | Value::False => {}
                // Only an absent or false marker is an ordinary row. Refuse all other
                // defaults, including malformed values, before the create can insert.
                _ => return Err(Error::user(GENERATED_INPUT_ERROR)),
            }
        }
        Ok(values)
    }

    /// Generated rows remain immutable even under System or full elevation.
    #[method(override)]
    async fn write(
        this: &RecordSet<IrAccess>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if has_generated(&vals) {
            return Err(Error::user(GENERATED_INPUT_ERROR));
        }
        refuse_generated(this.as_records(), tx).await?;
        sup.call(tx, this.as_records(), (vals,)).await
    }

    /// Archived generated rows and mixed recordsets are protected too.
    #[method(override)]
    async fn unlink(this: &RecordSet<IrAccess>, tx: &mut Transaction, sup: Super) -> Result<()> {
        refuse_generated(this.as_records(), tx).await?;
        sup.call(tx, this.as_records(), ()).await
    }
}

fn has_generated(vals: &RawVals) -> bool {
    vals.contains("generated") || vals.commands("generated").is_some()
}

async fn refuse_generated(records: &Records, tx: &mut Transaction) -> Result<()> {
    let ids = records.ids();
    if ids.is_empty() {
        return Ok(());
    }
    // Read the authoritative flag, not a record cache, an active-filtered search or the
    // caller's visible subset. A protected row anywhere in the batch refuses the whole call.
    // NULL is an ordinary legacy row, matching the policy loader's generated-column contract.
    let rows = tx
        .query(
            "SELECT m.model FROM ir_access a LEFT JOIN ir_model m ON m.id = a.model_id \
         WHERE a.id = ANY($1) AND a.generated ORDER BY a.id LIMIT 1",
            &[&ids],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let name: Option<String> = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
    let name = name.ok_or_else(|| Error::user(
        "A generated company restriction has no ir.model record; restore its model before updating access rows."
    ))?;
    let model = records.env().registry().model(&name).ok_or_else(|| Error::user(format!(
        "The generated company restriction of {name} has no registered model; update its owning addon before modifying access rows."
    )))?;
    let access = match model.company_access {
        erp_sdk::CompanyAccessState::Declared(access) if access.generates() => access,
        erp_sdk::CompanyAccessState::Declared(_)
        | erp_sdk::CompanyAccessState::Undeclared
        | erp_sdk::CompanyAccessState::NotApplicable(_) => {
            return Err(Error::user(format!(
                "The generated company restriction of {name} has no restricting company_access declaration; update its owning addon before modifying access rows."
            )));
        }
    };
    Err(Error::user(format!(
        "The company restriction of {name} is generated from its company_access declaration ({}, declared by addon '{}') and cannot be archived, changed or deleted; change the declaration and update the addon.",
        access.declaration().spelling(),
        access.source().addon(),
    )))
}
