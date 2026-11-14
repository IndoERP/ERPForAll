// SPDX-License-Identifier: MIT OR Apache-2.0
//! Guards around the quant engine: `stock.quant` balances are written only by
//! [`crate::quant_engine`], and a location's usage or active flag cannot change under its stock
//! (the location quant guards moved here, plan MF-7).

use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, Operation, ValueMap};

use crate::location::StockLocation;
use crate::quant::StockQuant;
use crate::quant_engine::QuantError;

/// Columns of the balance projection (9): only the engine writes them.
const PROJECTION_FIELDS: [&str; 9] = [
    "company_id",
    "product_id",
    "location_id",
    "lot_id",
    "package_id",
    "owner_id",
    "quantity",
    "reserved_quantity",
    "in_date",
];

#[methods(StockQuant, addon = "stock")]
impl StockQuant {
    /// Quants come from the engine only; the model access check runs first, so a user without
    /// create access still gets the access refusal.
    #[method(override)]
    async fn create(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
        _vals_list: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        Err(QuantError::EngineOnlyCreate.into())
    }

    /// Count state (counted quantity, assigned user) stays writable; the balance columns are refused
    /// by name.
    #[method(override)]
    async fn write(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if let Some(field) = PROJECTION_FIELDS.iter().find(|f| vals.contains(f)) {
            this.as_records().check_access(tx, Operation::Write).await?;
            let label = tx
                .registry()
                .model("stock.quant")
                .and_then(|model| model.field(field))
                .map(|spec| spec.string.clone())
                .ok_or_else(|| Error::internal(format!("stock.quant has no field '{field}'")))?;
            return Err(QuantError::EngineOnly { field: label }.into());
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    /// The generic upsert by the declared key runs no `create` or `write` override, so it is refused
    /// here, after the model access checks of both operations.
    #[method(override)]
    async fn upsert(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
        _call: erp_sdk::UpsertCall,
        _sup: Super,
    ) -> Result<erp_sdk::UpsertResult> {
        let records = this.as_records();
        records.check_access(tx, Operation::Create).await?;
        records.check_access(tx, Operation::Write).await?;
        Err(QuantError::EngineOnlyUpsert.into())
    }

    /// Quants cannot be duplicated; the create access check runs first.
    #[method(override)]
    async fn copy(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
        _default: Option<ValueMap>,
        _sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        Err(QuantError::Copy.into())
    }

    /// A quant with a balance is never deleted outside the engine, by any caller (9); a
    /// zero quant may be deleted by whoever the model access allows (the reference's stock
    /// manager; stock ships no access rows
    /// yet, so today only the superuser). The engine deletes its zero keys itself, in SQL.
    #[method(override)]
    async fn unlink(this: &RecordSet<StockQuant>, tx: &mut Transaction, sup: Super) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        if !this.is_empty() {
            let ids = this.ids();
            let rows = tx
                .query(
                    "SELECT EXISTS (SELECT FROM stock_quant WHERE id = ANY($1) \
                       AND (quantity <> 0 OR reserved_quantity <> 0))",
                    &[&ids],
                )
                .await?;
            let row = rows
                .first()
                .ok_or_else(|| Error::internal("quant unlink guard read returned no row"))?;
            let with_balance: bool = row
                .try_get(0)
                .map_err(|e| Error::internal(format!("quant unlink guard: {e}")))?;
            if with_balance {
                return Err(QuantError::UnlinkWithBalance.into());
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// `available_quantity`: on hand minus reserved, signed.
    #[compute(depends = ["quantity", "reserved_quantity"])]
    async fn _compute_available_quantity(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockQuant>,
    ) -> Result<()> {
        let quantities = this.mapped(tx, StockQuant::QUANTITY).await?;
        let reserved = this.mapped(tx, StockQuant::RESERVED_QUANTITY).await?;
        for (index, (quantity, reserved)) in quantities.into_iter().zip(reserved).enumerate() {
            out.set(index, StockQuant::AVAILABLE_QUANTITY, quantity - reserved)?;
        }
        Ok(())
    }
}

#[methods(StockLocation, addon = "stock")]
impl StockLocation {
    /// A location never changes company, a location holding quants cannot become a
    /// view, one holding any non-zero on-hand or reserved quantity cannot change its usage, and a
    /// location whose internal locations (itself or below) hold stock cannot be archived.
    /// The reads are SQL, so pending ORM writes are flushed first.
    #[method(override)]
    async fn write(
        this: &RecordSet<StockLocation>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        crate::location::check_ownership_assignment(this, tx, &vals).await?;
        if !this.is_empty() {
            check_company_change(this, tx, &vals).await?;
            // RPC values arrive converted (`null`, `0` become `Bool(false)`), a typed caller's do
            // not (`erp-orm` `write.rs:109-115`): anything but `true` is checked as an archive
            let archive = vals
                .get("active")
                .is_some_and(|active| !matches!(active, Cell::Bool(true)));
            if vals.contains("usage") || archive {
                tx.flush().await?;
                let sql = if archive {
                    LOCK_LOCATIONS_BELOW
                } else {
                    LOCK_LOCATIONS
                };
                lock_guarded(tx, sql, &this.ids(), "stock locations").await?;
            }
            check_usage_change(this, tx, &vals).await?;
            if archive {
                check_archive(this, tx).await?;
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await?;
        crate::res_company::invalidate_company_locations(tx).await
    }
}

/// The written locations, and for an archive every location below them too: the rows the engine's
/// validation read takes `FOR SHARE` on.
const LOCK_LOCATIONS: &str =
    "SELECT FROM stock_location WHERE id = ANY($1) ORDER BY id FOR UPDATE NOWAIT";
const LOCK_LOCATIONS_BELOW: &str = "SELECT FROM stock_location l WHERE EXISTS ( \
       SELECT FROM stock_location root WHERE root.id = ANY($1) \
          AND l.parent_path LIKE (root.parent_path || '%')) \
     ORDER BY l.id FOR UPDATE NOWAIT";

/// Lock the rows a guard decides on before it reads, without waiting (round 1 S6). A row a concurrent
/// stock change holds (the engine's `FOR SHARE` on its locations, a foreign-key check of a new row)
/// fails the lock with `55P03`; that is answered as a serialization failure (`40001`), so
/// [`Orm::run`](erp_sdk::Orm::run) retries the guard on a snapshot that sees the change once it is
/// committed. A stock change that starts after the lock waits for the guard's transaction and is then
/// retried on its write.
///
/// Not covered: a stock change that committed after this transaction's snapshot was taken and before
/// the lock. Its rows are not locked any more and not visible to the snapshot, so the guard decides
/// without it; closing that needs the invariant in a constraint the database checks across snapshots.
pub(crate) async fn lock_guarded(
    tx: &mut Transaction,
    sql: &str,
    ids: &[i64],
    what: &str,
) -> Result<()> {
    match tx.query(sql, &[&ids]).await {
        Ok(_) => Ok(()),
        Err(error) if error.sqlstate() == Some("55P03") => Err(Error::new(
            ErrorKind::Concurrency,
            format!(
                "could not serialize access: {what} {ids:?} are in use by a concurrent stock \
                 change"
            ),
        )
        .with_sqlstate(Some("40001".to_owned()))),
        Err(error) => Err(error),
    }
}

/// The reference compares each location's company with the written one;
/// an empty value (`Null`, or `Bool(false)` as
/// the RPC path converts it) is no company.
async fn check_company_change(
    this: &RecordSet<StockLocation>,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    let wanted = match vals.get("company_id") {
        None => return Ok(()),
        Some(Cell::Id(id)) => Some(*id),
        Some(Cell::Int(id)) => Some(i64::from(*id)),
        Some(Cell::Null | Cell::Bool(false)) => None,
        // any other cell is not a company id; refused here rather than guessed
        Some(_) => return Err(QuantError::CompanyChange.into()),
    };
    let current = this.mapped(tx, StockLocation::COMPANY_ID).await?;
    if current.iter().any(|c| c.map(|id| id.get()) != wanted) {
        return Err(QuantError::CompanyChange.into());
    }
    Ok(())
}

async fn check_usage_change(
    this: &RecordSet<StockLocation>,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    // any cell but a usage key (an empty or converted value) is checked as a change of every
    // location; the base write converts or refuses it afterwards
    let usage = match vals.get("usage") {
        None => return Ok(()),
        Some(Cell::Str(usage)) => Some(usage.to_string()),
        Some(_) => None,
    };
    let ids = this.ids();
    let current = this.mapped(tx, StockLocation::USAGE).await?;
    let changed: Vec<i64> = ids
        .iter()
        .zip(&current)
        .filter(|(_, old)| usage.is_none() || old.as_deref() != usage.as_deref())
        .map(|(id, _)| *id)
        .collect();
    let rows = tx
        .query(
            "SELECT EXISTS (SELECT FROM stock_quant WHERE location_id = ANY($1)), \
                    EXISTS (SELECT FROM stock_quant WHERE location_id = ANY($2) \
                              AND (quantity <> 0 OR reserved_quantity <> 0))",
            &[&ids, &changed],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::internal("location usage guard read returned no row"))?;
    let any: bool = row
        .try_get(0)
        .map_err(|e| Error::internal(format!("location usage guard: {e}")))?;
    let nonzero: bool = row
        .try_get(1)
        .map_err(|e| Error::internal(format!("location usage guard: {e}")))?;
    if usage.as_deref() == Some("view") && any {
        return Err(QuantError::UsageToView.into());
    }
    if nonzero {
        return Err(QuantError::UsageWithStock.into());
    }
    Ok(())
}

async fn check_archive(this: &RecordSet<StockLocation>, tx: &mut Transaction) -> Result<()> {
    let ids = this.ids();
    let rows = tx
        .query(
            "SELECT DISTINCT l.id, l.complete_name \
               FROM stock_location root \
               JOIN stock_location l ON l.parent_path LIKE (root.parent_path || '%') \
               JOIN stock_quant q ON q.location_id = l.id \
              WHERE root.id = ANY($1) AND l.usage = 'internal' \
                AND (q.quantity <> 0 OR q.reserved_quantity <> 0) \
              ORDER BY l.id",
            &[&ids],
        )
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut names = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: Option<String> = row
            .try_get(1)
            .map_err(|e| Error::internal(format!("location archive guard: {e}")))?;
        names.push(name.unwrap_or_default());
    }
    Err(QuantError::ArchiveWithStock {
        locations: names.join(", "),
    }
    .into())
}
