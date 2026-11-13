// SPDX-License-Identifier: MIT OR Apache-2.0
//! The stock quant engine: the one write path of `stock.quant` balances and the
//! availability reads over them (–D4, D3, D4, D11 as accepted with
//! amendments B1, B6; spec `STKQUANT`). Addon guide.
//!
//! Observed in the reference: one entry point changes on-hand or reserved quantity of one exact key; it
//! reads the matching rows without a lock, locks the first unlocked one with skip-locked and inserts a
//! second row when every row is locked, then a merge job folds the duplicates.
//! Adopted: the per-key balance,
//! signed changes, the incoming-date rule and the availability buckets of tracked products. Modified:
//! one row per key under `UNIQUE NULLS NOT DISTINCT`; a batch nets its changes per key and locks every
//! key in one ordered statement before it computes anything; the reserved counter is never clamped; no
//! balance is stored at a virtual location; a balance that reaches zero is deleted by the same call.
//!
//! **Lock order** (1, D1.3): the keys of one call are netted and sorted ascending over
//! `(company, product, location, lot, package, owner)` with an absent part after every present one
//! (PostgreSQL's `ASC NULLS LAST`), by the one comparator of the projection shape; step (a) locks them
//! in that order with an `INSERT … ON CONFLICT … DO UPDATE … WHERE FALSE` that inserts a zero row for a
//! missing key, so two calls over overlapping keys never wait on each other in opposite orders. Quant
//! keys are class 3 of the.1 hierarchy: a caller locks its picking, move and policy rows
//! before calling, and cost keys after.
//!
//! The order holds across calls only when a unit of work makes **one engine call**, or makes later calls
//! only with keys above the highest key it holds (4): the engine keeps the keys each
//! transaction locked and refuses a later call naming a lower key it does not hold yet, as a
//! `Programming` error naming both keys. Collect the changes of a unit of work and pass them in one
//! call. Count-state writes through the generic `write` lock in row order, not key order; they belong
//! before the engine call or in their own unit of work.
//!
//! **Access.** The engine reads and writes the quant table directly, as the reference's `sudo()` does:
//! it checks no model access and no record rule. It is for the stock transitions (move validation,
//! inventory application) that checked access on their own documents; it is never exposed as an RPC
//! method. Generic `create`, `write` of the balance columns and `copy` of `stock.quant` are refused
//! (`quant_guards`).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::num::NonZeroU32;

use erp_sdk::ErrorKind;
use erp_sdk::delta::{
    CanonicalKey, DeltaLimits, GrainColumn, GrainType, GrainValue, InsertFill, MeasureColumn,
    MeasureGuard, MeasureType, MeasureValue, ProjectionShape, SyncApply, apply_synchronous,
};
use erp_sdk::prelude::*;

/// The quant table, its business key and the decimal precision of its quantities.
const TABLE: &str = "stock_quant";
const KEY_CONSTRAINT: &str = "stock_quant_key";
const PRECISION_USAGE: &str = "Product Unit";
/// Significant digits of a quantity measure: the most an exact decimal holds.
const MEASURE_PRECISION: u32 = 28;
/// Location usages whose balances live in the ledger only (1; the reservation-bypass
/// usages).
const BYPASS_USAGES: [&str; 4] = ["supplier", "customer", "inventory", "production"];

/// The key of one balance: company, product, exact location and the optional lot, package and owner.
///
/// The company is the company of the ledger line that creates the key, never derived from the location
/// (B6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QuantKey {
    /// Company of the balance.
    pub company_id: i64,
    /// Product variant.
    pub product_id: i64,
    /// Exact location.
    pub location_id: i64,
    /// Lot or serial number; `None` for untracked stock.
    pub lot_id: Option<i64>,
    /// Package; `None` for loose stock.
    pub package_id: Option<i64>,
    /// Owner of consigned stock; `None` for the company's own stock.
    pub owner_id: Option<i64>,
}

/// A signed change of one key: an on-hand delta, a reserved delta and the incoming date the stock
/// carries (the source's date when stock moves between locations).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantChange {
    /// The balance it changes.
    pub key: QuantKey,
    /// On-hand delta in the product unit.
    pub quantity: Decimal,
    /// Reserved delta in the product unit.
    pub reserved_quantity: Decimal,
    /// Incoming date carried by the stock; `None` lets the key keep its own date or take now.
    pub in_date: Option<DateTime>,
}

/// A balance after a change, or as read. Only the engine builds one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuantBalance {
    /// Row id; `None` when the change brought the key to zero and its row was deleted.
    pub id: Option<i64>,
    /// The key.
    pub key: QuantKey,
    /// On-hand quantity.
    pub quantity: Decimal,
    /// Reserved quantity.
    pub reserved_quantity: Decimal,
    /// Incoming date of the key (for a deleted key, the date its stock carried).
    pub in_date: DateTime,
}

impl QuantBalance {
    /// On hand minus reserved, signed: the reference's strict availability that allows negatives.
    pub fn available(&self) -> Decimal {
        self.quantity - self.reserved_quantity
    }
}

/// Which balances an availability read covers (the reference's `_gather` domain,
/// ).
///
/// Broad (`strict = false`): the location and every descendant; a given lot also admits stock without
/// a lot; a given package or owner narrows, an absent one does not. Strict: the exact location, the
/// given lot or no lot, and exactly the given package and owner (absent means none). The company is a
/// part of every balance key (B6), so a read is always for one company.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantFilter {
    /// Company of the balances.
    pub company_id: i64,
    /// Product variant.
    pub product_id: i64,
    /// Location (with its descendants unless `strict`).
    pub location_id: i64,
    /// Lot wanted.
    pub lot_id: Option<i64>,
    /// Package wanted.
    pub package_id: Option<i64>,
    /// Owner wanted.
    pub owner_id: Option<i64>,
    /// Exact key parts instead of the broad match.
    pub strict: bool,
    /// The product is tracked by lot or serial number. Its `tracking` field is declared;
    /// until then the caller states it.
    pub tracked: bool,
}

/// A refused or failed quant operation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum QuantError {
    /// A change with neither quantity nor reserved quantity.
    EmptyChange,
    /// The product does not track inventory.
    NotStorable,
    /// A balance at a view location.
    ViewLocation {
        /// The location's name.
        location: String,
    },
    /// A lot of another product.
    LotOfOtherProduct {
        /// The lot's name.
        lot: String,
    },
    /// A key part that belongs to another company than the balance.
    OtherCompany {
        /// What the part is: product, location, lot or owner.
        what: &'static str,
        /// Its id.
        id: i64,
    },
    /// A key part that does not exist.
    Missing {
        /// What the part is.
        what: &'static str,
        /// Its id.
        id: i64,
    },
    /// A delta with more decimal digits than the Product Unit precision.
    Precision {
        /// `quantity` or `reserved_quantity`.
        field: &'static str,
        /// The delta.
        value: Decimal,
        /// The configured digits.
        digits: u32,
    },
    /// A release larger than the locked reserved counter (2): a defect, never clamped.
    ReservationUnderflow {
        /// The key.
        key: QuantKey,
        /// The locked counter.
        reserved: Decimal,
        /// The amount released.
        release: Decimal,
    },
    /// A statement wrote other rows or values than the locked rows predicted (3(b)).
    RowMismatch {
        /// What differed.
        detail: String,
    },
    /// An ORM write of a balance column (9); only
    /// [`apply_quant_changes`] writes them.
    EngineOnly {
        /// The label of the field written.
        field: String,
    },
    /// An ORM create of a quant; quants come from [`apply_quant_changes`] (counting new stock in
    /// inventory mode is's).
    EngineOnlyCreate,
    /// An ORM upsert of a quant by its key (the generic upsert runs no create or write override).
    EngineOnlyUpsert,
    /// A copy of a quant.
    Copy,
    /// An ORM deletion of a quant with an on-hand or reserved quantity (9; the
    /// reference zeroes it through an inventory adjustment first,
    /// which is's).
    UnlinkWithBalance,
    /// A location holding quants made a view.
    UsageToView,
    /// A location holding stock given another usage
    /// (here any non-zero on-hand or reserved
    /// quantity blocks, not only a positive one:).
    UsageWithStock,
    /// A location given another company.
    CompanyChange,
    /// A later engine call of the transaction names a key below one an earlier call locked
    /// (4).
    LockOrder {
        /// The new key.
        key: QuantKey,
        /// The highest key the transaction holds.
        held: QuantKey,
    },
    /// Archiving locations whose internal descendants hold stock.
    ArchiveWithStock {
        /// The full names of the locations holding stock, joined.
        locations: String,
    },
}

impl fmt::Display for QuantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyChange => f.write_str("Quantity or Reserved Quantity should be set."),
            Self::NotStorable => {
                f.write_str("Quants cannot be created for consumables or services.")
            }
            Self::ViewLocation { location } => write!(
                f,
                "You cannot take products from or deliver products to a location of type \
                 \"view\" ({location})."
            ),
            Self::LotOfOtherProduct { lot } => {
                write!(
                    f,
                    "The Lot/Serial number ({lot}) is linked to another product."
                )
            }
            Self::OtherCompany { what, id } => write!(
                f,
                "A stock balance cannot use {what} {id}: it belongs to another company than the \
                 balance."
            ),
            Self::Missing { what, id } => write!(
                f,
                "A stock balance change names {what} {id}, which does not exist."
            ),
            Self::Precision {
                field,
                value,
                digits,
            } => write!(
                f,
                "The {field} change {value} of a stock balance has more decimal digits than the \
                 {PRECISION_USAGE} precision allows ({digits})."
            ),
            Self::ReservationUnderflow {
                key,
                reserved,
                release,
            } => write!(
                f,
                "Reserved quantity of product {} at location {}{} would become negative ({reserved} \
                 reserved, {release} released). Nothing was changed. This is a defect; please \
                 report it.",
                key.product_id,
                key.location_id,
                key_parts(key),
                reserved = reserved.normalize(),
                release = release.normalize(),
            ),
            Self::RowMismatch { detail } => write!(
                f,
                "A stock balance write did not match the locked rows ({detail}). Nothing was saved. \
                 This is a defect; please report it."
            ),
            Self::EngineOnly { field } => write!(
                f,
                "{field} of a stock quant cannot be changed directly: balances change only through \
                 stock moves and inventory adjustments."
            ),
            Self::EngineOnlyCreate => f.write_str(
                "Stock quants cannot be created directly: balances change only through stock moves \
                 and inventory adjustments.",
            ),
            Self::EngineOnlyUpsert => f.write_str(
                "Stock quants are not created or updated by key: balances change only through stock \
                 moves and inventory adjustments.",
            ),
            Self::Copy => f.write_str("You cannot duplicate stock quants."),
            Self::UnlinkWithBalance => f.write_str(
                "A stock quant with an on-hand or reserved quantity cannot be deleted: its balance \
                 changes only through stock moves or an inventory adjustment.",
            ),
            Self::UsageToView => f.write_str(
                "This location's usage cannot be changed to view as it contains products.",
            ),
            Self::UsageWithStock => {
                f.write_str("Internal locations having stock can't be converted")
            }
            Self::CompanyChange => f.write_str(
                "Changing the company of this record is forbidden at this point, you should rather \
                 archive it and create a new one.",
            ),
            Self::LockOrder { key, held } => write!(
                f,
                "Stock balance keys are locked in ascending order within a transaction: key ({}) is \
                 below key ({}), which an earlier call of this transaction locked. Pass both changes \
                 in one call, or call in key order.",
                key_text(key),
                key_text(held)
            ),
            Self::ArchiveWithStock { locations } => write!(
                f,
                "You can't disable locations {locations} because they still contain products."
            ),
        }
    }
}

impl std::error::Error for QuantError {}

impl From<QuantError> for Error {
    fn from(error: QuantError) -> Self {
        let message = error.to_string();
        let base = match &error {
            QuantError::Missing { .. }
            | QuantError::ReservationUnderflow { .. }
            | QuantError::RowMismatch { .. }
            | QuantError::LockOrder { .. } => Self::programming(message),
            QuantError::Copy
            | QuantError::EngineOnly { .. }
            | QuantError::EngineOnlyCreate
            | QuantError::EngineOnlyUpsert
            | QuantError::CompanyChange
            | QuantError::UnlinkWithBalance
            | QuantError::UsageToView
            | QuantError::UsageWithStock
            | QuantError::ArchiveWithStock { .. } => Self::user(message),
            _ => Self::validation(message),
        };
        base.with_source(error)
    }
}

/// `" (lot 3, package 4, owner 5)"` for the present optional parts, else empty.
fn key_parts(key: &QuantKey) -> String {
    let parts: Vec<String> = [
        ("lot", key.lot_id),
        ("package", key.package_id),
        ("owner", key.owner_id),
    ]
    .into_iter()
    .filter_map(|(what, id)| id.map(|id| format!("{what} {id}")))
    .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

/// `"company 1, product 2, location 3, lot 4"`: every present part of a key.
fn key_text(key: &QuantKey) -> String {
    let mut text = format!(
        "company {}, product {}, location {}",
        key.company_id, key.product_id, key.location_id
    );
    for (what, id) in [
        ("lot", key.lot_id),
        ("package", key.package_id),
        ("owner", key.owner_id),
    ] {
        if let Some(id) = id {
            text.push_str(&format!(", {what} {id}"));
        }
    }
    text
}

/// The canonical order of keys (`ASC NULLS LAST` over the six parts), the one the lock step uses.
fn order(key: &QuantKey) -> impl Ord {
    let last = |v: Option<i64>| (v.is_none(), v.unwrap_or(0));
    (
        key.company_id,
        key.product_id,
        key.location_id,
        last(key.lot_id),
        last(key.package_id),
        last(key.owner_id),
    )
}

/// The keys a transaction's engine calls locked, and the highest of them (4). Recorded
/// only after a call succeeded; a later rollback to a savepoint taken before that call releases row
/// locks the record still names, which only makes the rule stricter than needed.
#[derive(Default)]
struct HeldKeys {
    keys: HashSet<QuantKey>,
    highest: Option<QuantKey>,
}

fn decode(error: impl fmt::Display) -> Error {
    Error::internal(format!(
        "stock quant engine: unreadable result column: {error}"
    ))
}

/// The retryable answer to a key another transaction deleted after this one's snapshot (see
/// [`LOCK_KEYS`]): `Concurrency` with SQLSTATE `40001`, which [`Orm::run`](erp_sdk::Orm::run) retries
/// with a fresh snapshot.
fn concurrent_delete(key: &QuantKey) -> Error {
    Error::new(
        ErrorKind::Concurrency,
        format!(
            "could not serialize access: the stock balance of product {} at location {}{} was \
             deleted by a concurrent transaction after this transaction started",
            key.product_id,
            key.location_id,
            key_parts(key)
        ),
    )
    .with_sqlstate(Some("40001".to_owned()))
}

fn mismatch(detail: impl Into<String>) -> Error {
    QuantError::RowMismatch {
        detail: detail.into(),
    }
    .into()
}

/// The Product Unit digits quantities are stored at.
async fn product_unit_digits(tx: &mut Transaction) -> Result<u32> {
    let precision = tx.env().records("decimal.precision")?;
    precision
        .call::<(String,), u32>(tx, "_precision_get", (PRECISION_USAGE.to_owned(),))
        .await
}

/// The projection shape of the quant table at `digits`: the six key columns and the two measures.
fn shape(digits: u32) -> Result<ProjectionShape> {
    let id = |name: &str, nullable: bool| GrainColumn::new(name, GrainType::Int8, nullable);
    let quantity = MeasureType::Numeric {
        precision: MEASURE_PRECISION,
        scale: digits,
    };
    ProjectionShape::new(
        "stock.quant",
        NonZeroU32::MIN,
        vec![
            id("company_id", false),
            id("product_id", false),
            id("location_id", false),
            id("lot_id", true),
            id("package_id", true),
            id("owner_id", true),
        ],
        vec![
            MeasureColumn::new("quantity", quantity),
            MeasureColumn::new("reserved_quantity", quantity),
        ],
        &DeltaLimits::default(),
    )
}

fn grain(key: &QuantKey) -> Vec<GrainValue> {
    let opt = |v: Option<i64>| v.map_or(GrainValue::Null, GrainValue::Int8);
    vec![
        GrainValue::Int8(key.company_id),
        GrainValue::Int8(key.product_id),
        GrainValue::Int8(key.location_id),
        opt(key.lot_id),
        opt(key.package_id),
        opt(key.owner_id),
    ]
}

fn key_of(canonical: &CanonicalKey) -> Result<QuantKey> {
    let values = canonical.values();
    let int = |i: usize| match values.get(i) {
        Some(GrainValue::Int8(v)) => Ok(Some(*v)),
        Some(GrainValue::Null) => Ok(None),
        _ => Err(Error::internal(
            "stock quant key has a value of another type",
        )),
    };
    let required =
        |i: usize| int(i)?.ok_or_else(|| Error::internal("stock quant key lost a required part"));
    Ok(QuantKey {
        company_id: required(0)?,
        product_id: required(1)?,
        location_id: required(2)?,
        lot_id: int(3)?,
        package_id: int(4)?,
        owner_id: int(5)?,
    })
}

fn numeric(values: &[MeasureValue], index: usize) -> Result<Decimal> {
    match values.get(index) {
        Some(MeasureValue::Numeric(v)) => Ok(*v),
        _ => Err(Error::internal("stock quant measure is not numeric")),
    }
}

/// Validation read of every distinct key: product storable and company, location usage and company,
/// lot product and company, owner company, package existence. One statement; names are read only to word a refusal.
const VALIDATE_KEYS: &str = "\
SELECT i.ord, i.company_id, i.product_id, i.location_id, i.lot_id, i.owner_id, \
       c.id IS NOT NULL, t.is_storable, (t.company_id IS NULL OR t.company_id = i.company_id), \
       l.usage, l.name, (l.company_id IS NULL OR l.company_id = i.company_id \
         OR ('/' || c.parent_path) LIKE ('%/' || l.company_id::text || '/%')), \
       lot.product_id, lot.name, (lot.company_id IS NULL OR lot.company_id = i.company_id), \
       o.id IS NOT NULL, (o.company_id IS NULL OR o.company_id = i.company_id), pk.id IS NOT NULL \
  FROM unnest($1::int8[], $2::int8[], $3::int8[], $4::int8[], $5::int8[], $6::int8[]) \
       WITH ORDINALITY AS i(company_id, product_id, location_id, lot_id, owner_id, package_id, ord) \
  LEFT JOIN res_company c ON c.id = i.company_id \
  LEFT JOIN product_product p ON p.id = i.product_id \
  LEFT JOIN product_template t ON t.id = p.product_tmpl_id \
  LEFT JOIN stock_location l ON l.id = i.location_id \
  LEFT JOIN stock_lot lot ON lot.id = i.lot_id \
  LEFT JOIN res_partner o ON o.id = i.owner_id \
  LEFT JOIN stock_package pk ON pk.id = i.package_id \
 ORDER BY i.ord";

/// Step (a).3: lock every key of the batch in its canonical order, inserting a zero row
/// for a missing key, and return the locked values (`inserted` rows are the zero rows). Under
/// `REPEATABLE READ` a row another transaction committed after the snapshot makes this statement fail
/// with `40001`, so the second branch's snapshot read is the locked version. A row another transaction
/// deleted after the snapshot is the other way round: the insert finds no conflict once that
/// transaction commits, and the second branch (which never sees the CTE's own insert) still reads the
/// deleted row from the snapshot, so the key comes back twice; the engine answers that with a
/// retryable `40001` of its own ([`concurrent_delete`]), because the snapshot would go on showing the
/// deleted row to every later read of the unit of work.
const LOCK_KEYS: &str = "\
WITH i AS MATERIALIZED (SELECT * FROM unnest($1::int8[], $2::int8[], $3::int8[], $4::int8[], \
       $5::int8[], $6::int8[]) WITH ORDINALITY \
       AS i(company_id, product_id, location_id, lot_id, package_id, owner_id, ord)), \
up AS (INSERT INTO stock_quant AS t (company_id, product_id, location_id, lot_id, package_id, \
       owner_id, quantity, reserved_quantity, in_date, create_uid, create_date, write_uid, write_date) \
       SELECT company_id, product_id, location_id, lot_id, package_id, owner_id, 0, 0, \
              (now() AT TIME ZONE 'UTC'), $7, (now() AT TIME ZONE 'UTC'), $7, \
              (now() AT TIME ZONE 'UTC') \
         FROM i ORDER BY ord \
       ON CONFLICT ON CONSTRAINT stock_quant_key DO UPDATE SET quantity = t.quantity WHERE FALSE \
       RETURNING t.id, t.company_id, t.product_id, t.location_id, t.lot_id, t.package_id, t.owner_id) \
SELECT i.ord, up.id, TRUE, 0::numeric, 0::numeric, NULL::timestamp, NULL::numeric, NULL::int8 \
  FROM up JOIN i ON up.company_id = i.company_id AND up.product_id = i.product_id \
   AND up.location_id = i.location_id AND up.lot_id IS NOT DISTINCT FROM i.lot_id \
   AND up.package_id IS NOT DISTINCT FROM i.package_id AND up.owner_id IS NOT DISTINCT FROM i.owner_id \
UNION ALL \
SELECT i.ord, q.id, FALSE, q.quantity, q.reserved_quantity, q.in_date, q.inventory_quantity, q.user_id \
  FROM i JOIN stock_quant q ON q.company_id = i.company_id AND q.product_id = i.product_id \
   AND q.location_id = i.location_id AND q.lot_id IS NOT DISTINCT FROM i.lot_id \
   AND q.package_id IS NOT DISTINCT FROM i.package_id AND q.owner_id IS NOT DISTINCT FROM i.owner_id \
 ORDER BY 1";

/// Step (c): the incoming date and write audit of every kept key, and the deletion of keys that ended
/// at zero without count state (5, D1.6). The delete re-checks the zero predicate on the
/// row; a key it does not delete is a mismatch.
const FINISH_KEYS: &str = "\
WITH i AS (SELECT * FROM unnest($1::int8[], $2::timestamp[], $3::bool[]) AS i(id, in_date, drop_key)), \
d AS (DELETE FROM stock_quant q USING i WHERE q.id = i.id AND i.drop_key \
        AND q.quantity = 0 AND q.reserved_quantity = 0 \
        AND COALESCE(q.inventory_quantity, 0) = 0 AND q.user_id IS NULL \
      RETURNING q.id), \
u AS (UPDATE stock_quant q SET in_date = COALESCE(i.in_date, (now() AT TIME ZONE 'UTC')), \
        write_uid = $4, write_date = (now() AT TIME ZONE 'UTC') \
        FROM i WHERE q.id = i.id AND NOT i.drop_key RETURNING q.id, q.in_date) \
SELECT d.id, COALESCE(i.in_date, (now() AT TIME ZONE 'UTC')), TRUE FROM d JOIN i ON i.id = d.id \
UNION ALL SELECT u.id, u.in_date, FALSE FROM u";

/// One key of a batch with what step (a) locked and what (a') decided.
struct Planned {
    key: QuantKey,
    id: i64,
    quantity: Decimal,
    reserved_quantity: Decimal,
    in_date: Option<DateTime>,
    drop_key: bool,
}

/// Apply signed changes to the balances of their keys, in one batch (2, D1.3).
///
/// Changes of one key are netted; keys at virtual locations (vendor, customer, inventory loss,
/// production) are dropped, their effect is in the ledger only (1); keys whose nets are
/// both zero are dropped. The rest are locked in key order in one statement (a zero row is inserted
/// for a new key), their new values are computed from the locked rows and refused before any write
/// when the reserved counter would go below zero, then written by the synchronous guarded apply of,
/// and finally given their incoming date, or deleted when they ended at zero with no count
/// state. Negative on-hand is allowed (the reference's default); the per-location guard that refuses
/// it is.
///
/// Incoming date of a key: the oldest of its own date (while its on-hand is positive) and the dates
/// the changes carry, else now.
///
/// Returns the balances of the stored keys, in lock order. Statements: one validation read and the
/// three of D1.3, whatever the batch size; the work runs in a savepoint, so a refusal leaves nothing
/// behind. The batch is bounded by the delta limits (2,000 distinct keys, 32,000 changes).
///
/// # Errors
///
/// Validation: [`QuantError::EmptyChange`], [`QuantError::NotStorable`],
/// [`QuantError::ViewLocation`], [`QuantError::LotOfOtherProduct`], [`QuantError::OtherCompany`],
/// [`QuantError::Precision`]. Programming: [`QuantError::Missing`],
/// [`QuantError::ReservationUnderflow`], [`QuantError::RowMismatch`], [`QuantError::LockOrder`] (a
/// later call below a held key, see the module's lock order). ResourceExhausted above the batch
/// limits. A serialization failure (`40001`) or deadlock is returned as is: the caller's unit of work
/// retries ([`Orm::run`](erp_sdk::Orm::run)).
pub async fn apply_quant_changes(
    tx: &mut Transaction,
    changes: &[QuantChange],
) -> Result<Vec<QuantBalance>> {
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    if changes
        .iter()
        .any(|c| c.quantity.is_zero() && c.reserved_quantity.is_zero())
    {
        return Err(QuantError::EmptyChange.into());
    }
    let digits = product_unit_digits(tx).await?;
    for change in changes {
        for (field, value) in [
            ("quantity", change.quantity),
            ("reserved_quantity", change.reserved_quantity),
        ] {
            if value.normalize().scale() > digits {
                return Err(QuantError::Precision {
                    field,
                    value,
                    digits,
                }
                .into());
            }
        }
    }
    // the reads below are SQL; the savepoint flushes every pending ORM write before it opens (a
    // location usage, a storable flag, count state written earlier in this transaction); the quant
    // cache is dropped before and after
    tx.invalidate_model("stock.quant", None).await?;
    let result = tx
        .savepoint(async |tx| apply_in_savepoint(tx, changes, digits).await)
        .await;
    tx.invalidate_model("stock.quant", None).await?;
    result
}

async fn apply_in_savepoint(
    tx: &mut Transaction,
    changes: &[QuantChange],
    digits: u32,
) -> Result<Vec<QuantBalance>> {
    let mut distinct: Vec<QuantKey> = Vec::new();
    let mut seen: HashSet<QuantKey> = HashSet::new();
    for change in changes {
        if seen.insert(change.key) {
            distinct.push(change.key);
        }
    }
    let stored = validate_keys(tx, &distinct).await?;
    let shape = shape(digits)?;
    let mut dates: HashMap<QuantKey, DateTime> = HashMap::new();
    let mut contributions = Vec::with_capacity(changes.len());
    for change in changes.iter().filter(|c| stored.contains(&c.key)) {
        if let Some(date) = change.in_date {
            dates
                .entry(change.key)
                .and_modify(|d| *d = (*d).min(date))
                .or_insert(date);
        }
        contributions.push((
            shape.admit_key(grain(&change.key))?,
            shape.admit_measures(vec![
                MeasureValue::Numeric(change.quantity),
                MeasureValue::Numeric(change.reserved_quantity),
            ])?,
        ));
    }
    let batch = shape.net(contributions, &DeltaLimits::default())?;
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let keys = batch
        .keys()
        .iter()
        .map(key_of)
        .collect::<Result<Vec<_>>>()?;
    let deltas = batch
        .vectors()
        .iter()
        .map(|v| {
            let values = shape.measure_values(v)?;
            Ok((numeric(&values, 0)?, numeric(&values, 1)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let uid = tx.env().uid();
    {
        let held = tx.extension::<HeldKeys>();
        if let Some(highest) = held.highest
            && let Some(key) = keys
                .iter()
                .find(|k| order(k) < order(&highest) && !held.keys.contains(k))
        {
            return Err(QuantError::LockOrder {
                key: *key,
                held: highest,
            }
            .into());
        }
    }

    // (a) lock every key in canonical order
    let column = |f: fn(&QuantKey) -> Option<i64>| keys.iter().map(f).collect::<Vec<_>>();
    let company = column(|k| Some(k.company_id));
    let product = column(|k| Some(k.product_id));
    let location = column(|k| Some(k.location_id));
    let lot = column(|k| k.lot_id);
    let package = column(|k| k.package_id);
    let owner = column(|k| k.owner_id);
    let rows = tx
        .query(
            LOCK_KEYS,
            &[&company, &product, &location, &lot, &package, &owner, &uid],
        )
        .await?;
    let mut ordinals = HashSet::with_capacity(rows.len());
    for row in &rows {
        let ordinal: i64 = row.try_get(0).map_err(decode)?;
        if !ordinals.insert(ordinal) {
            let key = usize::try_from(ordinal)
                .ok()
                .and_then(|o| o.checked_sub(1))
                .and_then(|i| keys.get(i))
                .ok_or_else(|| mismatch(format!("lock row {ordinal} names no key")))?;
            return Err(concurrent_delete(key));
        }
    }
    if rows.len() != keys.len() {
        return Err(mismatch(format!(
            "{} keys locked for {} keys",
            rows.len(),
            keys.len()
        )));
    }

    // (a') outcomes from the locked rows, refused before any write
    let mut planned = Vec::with_capacity(keys.len());
    for (index, row) in rows.iter().enumerate() {
        let ordinal: i64 = row.try_get(0).map_err(decode)?;
        if usize::try_from(ordinal).ok() != Some(index + 1) {
            return Err(mismatch(format!("lock row {ordinal} out of order")));
        }
        let key = keys[index];
        let (dq, dr) = deltas[index];
        let id: i64 = row.try_get(1).map_err(decode)?;
        let quantity: Decimal = row.try_get(3).map_err(decode)?;
        let reserved: Decimal = row.try_get(4).map_err(decode)?;
        let own_date: Option<DateTime> = row.try_get(5).map_err(decode)?;
        let counted: Option<Decimal> = row.try_get(6).map_err(decode)?;
        let counter: Option<i64> = row.try_get(7).map_err(decode)?;
        let overflow = || {
            mismatch(format!(
                "the balance of row {id} is out of the decimal range"
            ))
        };
        let new_quantity = quantity.checked_add(dq).ok_or_else(overflow)?;
        let new_reserved = reserved.checked_add(dr).ok_or_else(overflow)?;
        if new_reserved < Decimal::ZERO {
            return Err(QuantError::ReservationUnderflow {
                key,
                reserved,
                release: -dr,
            }
            .into());
        }
        let own = own_date.filter(|_| quantity > Decimal::ZERO);
        let in_date = match (own, dates.get(&key).copied()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        planned.push(Planned {
            key,
            id,
            quantity: new_quantity,
            reserved_quantity: new_reserved,
            in_date,
            drop_key: new_quantity.is_zero()
                && new_reserved.is_zero()
                && counted.is_none_or(|c| c.is_zero())
                && counter.is_none(),
        });
    }

    // (b) the synchronous guarded apply (amendment B1); the floor is the backstop of (a').
    // Every key exists after (a), but PostgreSQL checks NOT NULL on the proposed row before it
    // resolves the conflict, so the required incoming date gets an insert-only fill (B7);
    // (c) writes the real date.
    let registry = tx.registry().clone();
    let model = registry
        .model("stock.quant")
        .ok_or_else(|| Error::internal("stock quant engine: stock.quant is not in the registry"))?;
    let apply = SyncApply::new(
        &shape,
        TABLE,
        KEY_CONSTRAINT,
        vec![MeasureGuard::at_least(
            "reserved_quantity",
            MeasureValue::Numeric(Decimal::ZERO),
            "The reserved quantity of a stock balance cannot be negative.",
        )],
    )?
    .with_insert_fill(model, "in_date", InsertFill::TransactionTime)?;
    let applied = apply_synchronous(tx, &apply, &batch)
        .await
        .map_err(erp_sdk::delta::SyncApplyFailure::into_error)?;
    if applied.rows().len() != planned.len() {
        return Err(mismatch(format!(
            "{} rows applied for {} keys",
            applied.rows().len(),
            planned.len()
        )));
    }
    for (row, plan) in applied.rows().iter().zip(&planned) {
        let measures = row.measures();
        if row.inserted()
            || numeric(measures, 0)? != plan.quantity
            || numeric(measures, 1)? != plan.reserved_quantity
        {
            return Err(mismatch(format!(
                "key of row {} was applied otherwise",
                plan.id
            )));
        }
    }

    // (c) incoming dates and zero-key deletion
    let ids: Vec<i64> = planned.iter().map(|p| p.id).collect();
    let in_dates: Vec<Option<DateTime>> = planned.iter().map(|p| p.in_date).collect();
    let drops: Vec<bool> = planned.iter().map(|p| p.drop_key).collect();
    let finished = tx
        .query(FINISH_KEYS, &[&ids, &in_dates, &drops, &uid])
        .await?;
    let mut outcome: HashMap<i64, (DateTime, bool)> = HashMap::with_capacity(finished.len());
    for row in &finished {
        let id: i64 = row.try_get(0).map_err(decode)?;
        let in_date: DateTime = row.try_get(1).map_err(decode)?;
        let dropped: bool = row.try_get(2).map_err(decode)?;
        outcome.insert(id, (in_date, dropped));
    }
    let balances = planned
        .into_iter()
        .map(|plan| {
            let Some((in_date, dropped)) = outcome.get(&plan.id).copied() else {
                return Err(mismatch(format!("row {} was not finished", plan.id)));
            };
            if dropped != plan.drop_key {
                return Err(mismatch(format!("row {} was not deleted as zero", plan.id)));
            }
            Ok(QuantBalance {
                id: (!dropped).then_some(plan.id),
                key: plan.key,
                quantity: plan.quantity,
                reserved_quantity: plan.reserved_quantity,
                in_date,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let held = tx.extension::<HeldKeys>();
    held.keys.extend(balances.iter().map(|b| b.key));
    held.highest = balances
        .iter()
        .map(|b| b.key)
        .chain(held.highest)
        .max_by(|a, b| order(a).cmp(&order(b)));
    Ok(balances)
}

/// Validate the distinct keys of a batch; returns the keys whose balances are stored (internal and
/// transit locations), keyed for lookup.
async fn validate_keys(tx: &mut Transaction, keys: &[QuantKey]) -> Result<HashSet<QuantKey>> {
    let company: Vec<i64> = keys.iter().map(|k| k.company_id).collect();
    let product: Vec<i64> = keys.iter().map(|k| k.product_id).collect();
    let location: Vec<i64> = keys.iter().map(|k| k.location_id).collect();
    let lot: Vec<Option<i64>> = keys.iter().map(|k| k.lot_id).collect();
    let owner: Vec<Option<i64>> = keys.iter().map(|k| k.owner_id).collect();
    let package: Vec<Option<i64>> = keys.iter().map(|k| k.package_id).collect();
    let rows = tx
        .query(
            VALIDATE_KEYS,
            &[&company, &product, &location, &lot, &owner, &package],
        )
        .await?;
    if rows.len() != keys.len() {
        return Err(Error::internal(
            "stock quant validation read lost keys of the batch",
        ));
    }
    let mut stored = HashSet::new();
    for (key, row) in keys.iter().zip(&rows) {
        let company_found: bool = row.try_get(6).map_err(decode)?;
        if !company_found {
            return Err(QuantError::Missing {
                what: "company",
                id: key.company_id,
            }
            .into());
        }
        let Some(storable) = row.try_get::<_, Option<bool>>(7).map_err(decode)? else {
            return Err(QuantError::Missing {
                what: "product",
                id: key.product_id,
            }
            .into());
        };
        if !storable {
            return Err(QuantError::NotStorable.into());
        }
        if row.try_get::<_, Option<bool>>(8).map_err(decode)? != Some(true) {
            return Err(QuantError::OtherCompany {
                what: "product",
                id: key.product_id,
            }
            .into());
        }
        let Some(usage) = row.try_get::<_, Option<String>>(9).map_err(decode)? else {
            return Err(QuantError::Missing {
                what: "location",
                id: key.location_id,
            }
            .into());
        };
        if usage == "view" {
            let location: Option<String> = row.try_get(10).map_err(decode)?;
            return Err(QuantError::ViewLocation {
                location: location.unwrap_or_default(),
            }
            .into());
        }
        if row.try_get::<_, Option<bool>>(11).map_err(decode)? != Some(true) {
            return Err(QuantError::OtherCompany {
                what: "location",
                id: key.location_id,
            }
            .into());
        }
        if let Some(lot) = key.lot_id {
            let Some(lot_product) = row.try_get::<_, Option<i64>>(12).map_err(decode)? else {
                return Err(QuantError::Missing {
                    what: "lot",
                    id: lot,
                }
                .into());
            };
            if lot_product != key.product_id {
                let name: Option<String> = row.try_get(13).map_err(decode)?;
                return Err(QuantError::LotOfOtherProduct {
                    lot: name.unwrap_or_default(),
                }
                .into());
            }
            if row.try_get::<_, Option<bool>>(14).map_err(decode)? != Some(true) {
                return Err(QuantError::OtherCompany {
                    what: "lot",
                    id: lot,
                }
                .into());
            }
        }
        if let Some(owner) = key.owner_id {
            if !row.try_get::<_, bool>(15).map_err(decode)? {
                return Err(QuantError::Missing {
                    what: "owner",
                    id: owner,
                }
                .into());
            }
            if row.try_get::<_, Option<bool>>(16).map_err(decode)? != Some(true) {
                return Err(QuantError::OtherCompany {
                    what: "owner",
                    id: owner,
                }
                .into());
            }
        }
        if let Some(package) = key.package_id
            && !row.try_get::<_, bool>(17).map_err(decode)?
        {
            return Err(QuantError::Missing {
                what: "package",
                id: package,
            }
            .into());
        }
        if !BYPASS_USAGES.contains(&usage.as_str()) {
            stored.insert(*key);
        }
    }
    Ok(stored)
}

/// The `WHERE` of a read, over `q` (quants) and `l` (their location), with parameters `$1` company,
/// `$2` product, `$3` location, `$4` lot, `$5` package, `$6` owner. `skip_lotless` drops stock without
/// a lot (a strict read of a tracked product's lot).
fn read_predicate(strict: bool, skip_lotless: bool) -> String {
    let mut parts = vec!["q.company_id = $1", "q.product_id = $2"];
    if strict {
        parts.extend([
            "q.location_id = $3",
            "(q.lot_id IS NULL OR q.lot_id = $4)",
            "q.package_id IS NOT DISTINCT FROM $5",
            "q.owner_id IS NOT DISTINCT FROM $6",
        ]);
    } else {
        parts.extend([
            "l.parent_path LIKE (root.parent_path || '%')",
            "($4::int8 IS NULL OR q.lot_id IS NULL OR q.lot_id = $4)",
            "($5::int8 IS NULL OR q.package_id = $5)",
            "($6::int8 IS NULL OR q.owner_id = $6)",
        ]);
    }
    if skip_lotless {
        parts.push("q.lot_id IS NOT NULL");
    }
    parts.join(" AND ")
}

fn missing_location(filter: &QuantFilter) -> Error {
    QuantError::Missing {
        what: "location",
        id: filter.location_id,
    }
    .into()
}

/// The balances a filter covers (the reference's `_gather`), lot-bearing balances first, then by id.
/// The removal-strategy order is's. One statement, no lock, no write.
///
/// # Errors
///
/// Programming [`QuantError::Missing`] for a location that does not exist.
pub async fn gather_quants(
    tx: &mut Transaction,
    filter: &QuantFilter,
) -> Result<Vec<QuantBalance>> {
    tx.flush().await?;
    // no row: the location does not exist; one row of NULLs: it holds no matching balance
    let sql = format!(
        "WITH root AS (SELECT parent_path FROM stock_location WHERE id = $3) \
         SELECT g.* FROM root LEFT JOIN LATERAL ( \
           SELECT q.id, q.company_id, q.product_id, q.location_id, q.lot_id, q.package_id, \
                  q.owner_id, q.quantity, q.reserved_quantity, q.in_date \
             FROM stock_quant q JOIN stock_location l ON l.id = q.location_id \
            WHERE {}) g ON TRUE \
          ORDER BY g.lot_id IS NULL, g.id",
        read_predicate(filter.strict, false)
    );
    let rows = tx
        .query(
            &sql,
            &[
                &filter.company_id,
                &filter.product_id,
                &filter.location_id,
                &filter.lot_id,
                &filter.package_id,
                &filter.owner_id,
            ],
        )
        .await?;
    if rows.is_empty() {
        return Err(missing_location(filter));
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(id) = row.try_get::<_, Option<i64>>(0).map_err(decode)? else {
            continue;
        };
        out.push(QuantBalance {
            id: Some(id),
            key: QuantKey {
                company_id: row.try_get(1).map_err(decode)?,
                product_id: row.try_get(2).map_err(decode)?,
                location_id: row.try_get(3).map_err(decode)?,
                lot_id: row.try_get(4).map_err(decode)?,
                package_id: row.try_get(5).map_err(decode)?,
                owner_id: row.try_get(6).map_err(decode)?,
            },
            quantity: row.try_get(7).map_err(decode)?,
            reserved_quantity: row.try_get(8).map_err(decode)?,
            in_date: row.try_get(9).map_err(decode)?,
        });
    }
    Ok(out)
}

/// Available quantity of a filter: on hand minus reserved over its balances (the reference's
/// `_get_available_quantity`).
///
/// Untracked: the sum, or zero when it is negative unless `allow_negative`. Tracked: each lot nets
/// separately and stock without a lot is one more bucket (left out by a strict read of a lot); without
/// `allow_negative` only positive buckets count, with it every bucket does. Comparisons are exact at the
/// stored digits. One statement, no lock, no write.
///
/// # Errors
///
/// Programming [`QuantError::Missing`] for a location that does not exist.
pub async fn available_quantity(
    tx: &mut Transaction,
    filter: &QuantFilter,
    allow_negative: bool,
) -> Result<Decimal> {
    tx.flush().await?;
    let skip_lotless = filter.tracked && filter.strict && filter.lot_id.is_some();
    let sql = format!(
        "WITH root AS (SELECT parent_path FROM stock_location WHERE id = $3), \
         g AS (SELECT q.lot_id, sum(q.quantity - q.reserved_quantity) AS free \
                 FROM root, stock_quant q JOIN stock_location l ON l.id = q.location_id \
                WHERE {} GROUP BY q.lot_id) \
         SELECT (SELECT count(*) FROM root), COALESCE((SELECT sum(free) FROM g), 0), \
                COALESCE((SELECT sum(free) FROM g WHERE free > 0), 0)",
        read_predicate(filter.strict, skip_lotless)
    );
    let row = tx
        .query(
            &sql,
            &[
                &filter.company_id,
                &filter.product_id,
                &filter.location_id,
                &filter.lot_id,
                &filter.package_id,
                &filter.owner_id,
            ],
        )
        .await?;
    let row = row
        .first()
        .ok_or_else(|| Error::internal("stock availability read returned no row"))?;
    if row.try_get::<_, i64>(0).map_err(decode)? == 0 {
        return Err(missing_location(filter));
    }
    let total: Decimal = row.try_get(1).map_err(decode)?;
    let positive: Decimal = row.try_get(2).map_err(decode)?;
    Ok(match (allow_negative, filter.tracked) {
        (true, _) => total,
        (false, true) => positive,
        (false, false) => total.max(Decimal::ZERO),
    })
}
