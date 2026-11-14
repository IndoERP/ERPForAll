// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company stock links. WH-STEPS owns topology provisioning; an empty lookup stays empty.
use crate::error::StockError;
use erp_addon_base::ResCompany;
use erp_sdk::{Operation, prelude::*};
use std::collections::BTreeMap;

#[extend(ResCompany, addon = "stock", check_company_auto = true)]
struct StockCompany {
    /// Transit within this company; deleting its location is refused while referenced.
    #[field(
        many2one,
        comodel = "stock.location",
        ondelete = "restrict",
        check_company = true,
        string = "Internal transit location"
    )]
    pub internal_transit_location_id: Option<RecId>,
    // Nonstored outputs have no assignable relation to validate in the generic company
    // writer: the exact-company lookup domain and the assignment fence enforce their scope.
    /// First active company inventory-loss location by ID. Derived, not a writable setting.
    #[field(
        many2one,
        comodel = "stock.location",
        compute = "_compute_stock_company_locations",
        readonly = true,
        string = "Scrap location"
    )]
    pub scrap_location_id: Option<RecId>,
    /// Stock location of the first active company warehouse in warehouse order.
    #[field(
        many2one,
        comodel = "stock.location",
        compute = "_compute_stock_company_locations",
        readonly = true,
        string = "Default stock location"
    )]
    pub default_stock_location_id: Option<RecId>,
}

#[methods(ResCompany, addon = "stock")]
impl StockCompany {
    #[compute(depends_context = ["uid", "allowed_company_ids", "active_test"])]
    async fn _compute_stock_company_locations(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResCompany>,
    ) -> Result<()> {
        // Batch candidate reads keep the caller's access. Explicit active predicates make this
        // default independent of a caller's active_test=False history browsing context.
        // Draft IDs cannot own persisted locations; only real company IDs enter the query.
        let companies: Vec<i64> = this
            .rec_ids()
            .iter()
            .filter_map(|id| id.as_real().map(|r| r.get()))
            .collect();
        let scope = Domain::condition("company_id", Operator::In, companies)?;
        let active = Domain::condition("active", Operator::Eq, true)?;
        let locations = this
            .env()
            .records("stock.location")?
            .search_with(
                tx,
                &Domain::and([
                    scope.clone(),
                    active.clone(),
                    Domain::condition("usage", Operator::Eq, "inventory")?,
                ]),
                &SearchOptions::new()
                    .order("id")
                    .fetch_fields(["company_id"]),
            )
            .await?;
        let mut scrap = BTreeMap::new();
        for (id, company) in locations
            .rec_ids()
            .iter()
            .copied()
            .zip(locations.cells(tx, "company_id").await?)
        {
            match company {
                Cell::Id(company) => {
                    scrap.entry(company).or_insert(id);
                }
                _ => {
                    return Err(Error::programming(
                        "A company inventory location has no valid company.",
                    ));
                }
            }
        }
        let warehouses = this
            .env()
            .records("stock.warehouse")?
            .search_with(
                tx,
                &Domain::and([scope, active]),
                &SearchOptions::new()
                    .order("sequence, id")
                    .fetch_fields(["company_id", "lot_stock_id"]),
            )
            .await?;
        let mut stock = BTreeMap::new();
        for (company, location) in warehouses
            .cells(tx, "company_id")
            .await?
            .into_iter()
            .zip(warehouses.cells(tx, "lot_stock_id").await?)
        {
            match (company, location) {
                (Cell::Id(company), Cell::Id(location)) => {
                    let location = RecId::from_real(location).ok_or_else(|| {
                        Error::programming("A warehouse has an invalid stock location.")
                    })?;
                    stock.entry(company).or_insert(location);
                }
                _ => {
                    return Err(Error::programming(
                        "A warehouse has no valid company or stock location.",
                    ));
                }
            }
        }
        for (index, id) in this.rec_ids().iter().enumerate() {
            // A draft company has no generated topology; absence is the specified default.
            let scrap = id.as_real().and_then(|id| scrap.get(&id.get()).copied());
            let stock = id.as_real().and_then(|id| stock.get(&id.get()).copied());
            out.set(index, StockCompany::SCRAP_LOCATION_ID, scrap)?;
            out.set(index, StockCompany::DEFAULT_STOCK_LOCATION_ID, stock)?;
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        // a `default_<field>` context key or a stored `ir.default` would be filled in by the base
        // create after this check; `default_get` reports both
        let defaulted = !this
            .as_records()
            .default_get(tx, &DERIVED)
            .await?
            .is_empty();
        if defaulted || vals_list.iter().any(has_derived_assignment) {
            this.as_records()
                .check_access(tx, Operation::Create)
                .await?;
            for vals in &vals_list {
                check_supplied_fields(this, tx, vals).await?;
            }
            return Err(StockError::CompanyLocationsDerived.into());
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.is_empty() && has_derived_assignment(&vals) {
            this.as_records().check_access(tx, Operation::Write).await?;
            check_supplied_fields(this, tx, &vals).await?;
            return Err(StockError::CompanyLocationsDerived.into());
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }
}
const DERIVED: [&str; 2] = ["scrap_location_id", "default_stock_location_id"];
fn has_derived_assignment(vals: &RawVals) -> bool {
    DERIVED
        .iter()
        .any(|f| vals.contains(f) || vals.iter_commands().any(|(name, _)| name == *f))
}
async fn check_supplied_fields(
    this: &RecordSet<ResCompany>,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    for field in vals
        .iter()
        .map(|(field, _)| field)
        .chain(vals.iter_commands().map(|(field, _)| field))
    {
        if this.as_records().spec().field(field).is_some()
            && !tx
                .has_field_access(this.env(), "res.company", field, FieldOp::Write)
                .await?
        {
            return Err(Error::access(format!(
                "Writing res.company.{field} is not allowed."
            )));
        }
    }
    Ok(())
}

/// All derived values are transaction-local. Mutation owners call this only after successful
/// delegation; savepoint rollback restores the cache through the ordinary transaction machinery.
pub(crate) async fn invalidate_company_locations(tx: &mut Transaction) -> Result<()> {
    tx.invalidate_model(
        "res.company",
        Some(&["scrap_location_id", "default_stock_location_id"]),
    )
    .await
}
