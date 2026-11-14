// SPDX-License-Identifier: MIT OR Apache-2.0
//! Engine-owned columns of moves and move lines (lead ruling L6'): the done time,
//! the open/done membership, the correction counter and the move's open-line version (
//! "Writers") are written only by the stock engine's SQL statements, never through the ORM.
//! `readonly` binds the client only, so an ORM or RPC `create` or `write` naming one of them, or a
//! `create` whose context carries a `default_<field>` key for one, is refused here, after the model
//! access check, in the pattern of the quant balance guard (`quant_guards`). Every create row then
//! gets the columns' initial values, so no stored `ir.default` can supply them either. The engine
//! never reaches these overrides.

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::error::StockError;
use crate::stock_move::StockMove;
use crate::stock_move_line::StockMoveLine;

const MOVE_FIELDS: [&str; 2] = ["done_at", "open_line_version"];
const LINE_FIELDS: [&str; 3] = ["done_at", "membership", "correction_seq"];

fn label(tx: &Transaction, model: &str, field: &str) -> Result<String> {
    tx.registry()
        .model(model)
        .and_then(|spec| spec.field(field))
        .map(|spec| spec.string.clone())
        .ok_or_else(|| Error::internal(format!("{model} has no field '{field}'")))
}

/// The first engine-owned field `vals` names, as a refusal with its label.
fn refuse(
    tx: &Transaction,
    model: &str,
    what: &'static str,
    fields: &[&str],
    vals: &RawVals,
) -> Result<()> {
    let Some(field) = fields.iter().find(|f| vals.contains(f)) else {
        return Ok(());
    };
    let field = label(tx, model, field)?;
    Err(StockError::EngineOnlyField { what, field }.into())
}

/// The value a new row starts with: the declared defaults (`open`, 0) and an empty done time.
fn initial(field: &str) -> Result<Cell> {
    match field {
        "membership" => Ok(Cell::Str("open".into())),
        "correction_seq" | "open_line_version" => Ok(Cell::Int(0)),
        "done_at" => Ok(Cell::Null),
        other => Err(Error::internal(format!(
            "no initial value for the engine-owned field '{other}'"
        ))),
    }
}

async fn check_create(
    records: &Records,
    tx: &mut Transaction,
    model: &str,
    what: &'static str,
    fields: &[&'static str],
    vals_list: &mut [RawVals],
) -> Result<()> {
    let context = records.env().context();
    let defaulted = fields
        .iter()
        .copied()
        .find(|f| context.contains(&format!("default_{f}")));
    if defaulted.is_some()
        || vals_list
            .iter()
            .any(|vals| fields.iter().any(|f| vals.contains(f)))
    {
        records.check_access(tx, Operation::Create).await?;
        if let Some(field) = defaulted {
            let field = label(tx, model, field)?;
            return Err(StockError::EngineOnlyDefault { what, field }.into());
        }
        for vals in vals_list.iter() {
            refuse(tx, model, what, fields, vals)?;
        }
    }
    for vals in vals_list {
        for field in fields {
            vals.insert((*field).into(), initial(field)?);
        }
    }
    Ok(())
}

async fn check_write(
    records: &Records,
    tx: &mut Transaction,
    model: &str,
    what: &'static str,
    fields: &[&str],
    vals: &RawVals,
) -> Result<()> {
    if fields.iter().any(|f| vals.contains(f)) {
        records.check_access(tx, Operation::Write).await?;
        refuse(tx, model, what, fields, vals)?;
    }
    Ok(())
}

#[methods(StockMove, addon = "stock")]
impl StockMove {
    /// The done time and the open-line version come from the engine; refused by name or context
    /// default after the create access check, then set to their initial values.
    #[method(override)]
    async fn create(
        this: &RecordSet<StockMove>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let records = this.as_records();
        check_create(
            records,
            tx,
            "stock.move",
            "stock move",
            &MOVE_FIELDS,
            &mut vals_list,
        )
        .await?;
        sup.call(tx, records, (vals_list,)).await
    }

    /// The done time and the open-line version come from the engine; refused by name after the
    /// write access check.
    #[method(override)]
    async fn write(
        this: &RecordSet<StockMove>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        check_write(records, tx, "stock.move", "stock move", &MOVE_FIELDS, &vals).await?;
        sup.call(tx, records, (vals,)).await
    }
}

#[methods(StockMoveLine, addon = "stock")]
impl StockMoveLine {
    /// Done time, membership and correction counter come from the engine; refused by name or context
    /// default after the create access check, then set to their initial values.
    #[method(override)]
    async fn create(
        this: &RecordSet<StockMoveLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let records = this.as_records();
        check_create(
            records,
            tx,
            "stock.move.line",
            "stock move line",
            &LINE_FIELDS,
            &mut vals_list,
        )
        .await?;
        sup.call(tx, records, (vals_list,)).await
    }

    /// Done time, membership and correction counter come from the engine; refused by name after the
    /// write access check.
    #[method(override)]
    async fn write(
        this: &RecordSet<StockMoveLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        check_write(
            records,
            tx,
            "stock.move.line",
            "stock move line",
            &LINE_FIELDS,
            &vals,
        )
        .await?;
        sup.call(tx, records, (vals,)).await
    }
}
