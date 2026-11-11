//! Lookups of seeded records by external id, shared by the models of this addon.

use erp_sdk::prelude::*;

pub const DEFAULT_UNIT_XMLID: (&str, &str) = ("uom", "product_uom_unit");

/// # Errors
///
/// `Programming` when the registry has no `ir.model.data` model (the data addon is not composed);
/// `Validation` when the external id is missing, which means the `uom` data files did not load or
/// the unit was removed. There is no fallback to another unit and no unit is created here.
pub async fn default_unit_id(tx: &mut Transaction) -> Result<i64> {
    let (module, name) = DEFAULT_UNIT_XMLID;
    let Some(data) = tx.env().registry().model("ir.model.data").map(|m| m.id) else {
        return Err(Error::programming(
            "the default unit of measure is looked up by external id uom.product_uom_unit, but \
             this registry has no ir.model.data model: compose the data addon with base",
        ));
    };
    // an external id created earlier in this transaction may still be a pending ORM write
    tx.flush_model(data).await?;
    let rows = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module = $1 AND name = $2 AND model = 'uom.uom'",
            &[&module, &name],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::validation(format!(
            "The default unit of measure {module}.{name} does not exist: install the uom data \
             files or give the product a unit of measure."
        ))
    })?;
    let id: Option<i64> = row.try_get(0).map_err(|e| {
        Error::db(format!(
            "unexpected value in ir_model_data.res_id of {module}.{name}: {e}"
        ))
    })?;
    id.ok_or_else(|| Error::validation(format!("The external id {module}.{name} names no record.")))
}

/// The active company of the environment, or a clear refusal. A record that is scoped to a
/// company never falls back to "company 1".
///
/// # Errors
///
/// `Validation` when the environment has no company.
pub fn env_company_id(env: &Env, what: &str) -> Result<i64> {
    env.company_id()?.ok_or_else(|| {
        Error::validation(format!(
            "No company is active, so {what} cannot be determined: set the company of the \
             record or run in an environment that has a company."
        ))
    })
}

/// The currency of company `company_id`.
///
/// # Errors
///
/// `Validation` when the company has no currency; read errors propagate.
pub async fn company_currency_id(tx: &mut Transaction, env: &Env, company_id: i64) -> Result<i64> {
    let company = env.records("res.company")?.browse(vec![company_id])?;
    match company.cell(tx, "currency_id").await? {
        Cell::Id(id) => Ok(id),
        other => Err(Error::validation(format!(
            "Company {company_id} has no currency (found {other:?})."
        ))),
    }
}

/// A many2one cell as an optional id: `Null` is `None`, anything else than an id is an error.
///
/// # Errors
///
/// `Programming` when the cell is neither an id nor null (a field of another type was read).
pub fn opt_id(cell: Cell, field: &str) -> Result<Option<i64>> {
    match cell {
        Cell::Id(id) => Ok(Some(id)),
        Cell::Null => Ok(None),
        other => Err(Error::programming(format!(
            "field {field} returned {other:?}, expected an id or null"
        ))),
    }
}
