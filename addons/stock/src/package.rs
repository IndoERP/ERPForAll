// SPDX-License-Identifier: MIT OR Apache-2.0
//! Packages (`stock.package`): table only. The stored computes over contained
//! quants and child packages (`complete_name`, `location_id`, `company_id`) and the name sequence
//! default are's.

use erp_sdk::sequence::draw_by_code;
use erp_sdk::{CompanyId, Operation, ValueMap, prelude::*};

use crate::picking::StockPicking;

/// `stock.package`: a physical or logical container of quants and other packages.
#[model(
    name = "stock.package",
    description = "Package",
    order = "name, id",
    parent_store = true,
    parent_name = "parent_package_id",
    company_access = unrestricted(reason = "packages are physical containers whose quants carry company scope")
)]
pub struct StockPackage {
    /// Package reference.
    #[field(
        char,
        required = true,
        default = "New",
        copy = false,
        index = "trigram",
        string = "Package Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Package type (dimensions, weights, sequence).
    #[field(
        many2one,
        comodel = "stock.package.type",
        index = true,
        string = "Package Type"
    )]
    pub package_type_id: Option<RecId>,
    /// Container this package is in.
    #[field(
        many2one,
        comodel = "stock.package",
        index = "btree_not_null",
        string = "Container"
    )]
    pub parent_package_id: Option<RecId>,
    /// Packages contained in this one.
    #[field(
        one2many,
        comodel = "stock.package",
        inverse_name = "parent_package_id",
        string = "Contained Packages"
    )]
    pub child_package_ids: Vec<RecId>,
    /// Container the package goes into at its destination.
    #[field(
        many2one,
        comodel = "stock.package",
        index = "btree_not_null",
        string = "Destination Container"
    )]
    pub package_dest_id: Option<RecId>,
    /// Materialized ancestry path maintained by the ORM parent store.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
    /// Total weight of the package, when set by hand.
    #[field(float, digits = "Stock Weight", string = "Shipping Weight")]
    pub shipping_weight: Decimal,
    /// Date the package was packed.
    #[field(date, string = "Pack Date")]
    pub pack_date: Option<Date>,
}

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("A stock package record could not be read.").with_source(e)
}

async fn next_package_name(env: &Env, tx: &mut Transaction) -> Result<String> {
    let today = env.context_today()?;
    if let Some(cid) = env.company_id()? {
        let company = CompanyId::new(cid)?;
        return draw_by_code(tx, &env.sudo(true), "stock.package", company, Some(today)).await;
    }
    let rows = tx
        .query(
            "SELECT nextval(pg_get_serial_sequence('stock_package', 'id'))::int8",
            &[],
        )
        .await?;
    let next: i64 = rows
        .first()
        .ok_or_else(|| Error::internal("Package sequence returned no row."))?
        .try_get(0)
        .map_err(db_err)?;
    Ok(format!("PACK{next:07}"))
}

#[methods(StockPackage, addon = "stock")]
impl StockPackage {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<StockPackage>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut out: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        if fields.iter().any(|f| f == "pack_date") && !out.contains_key("pack_date") {
            let today = match this.env().company_id()? {
                Some(cid) => erp_addon_base::company_calendar::business_date(tx, cid).await?,
                None => this.env().context_today()?,
            };
            out.insert("pack_date".into(), Value::Date(today));
        }
        Ok(out)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<StockPackage>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        let env = this.env();
        let today = match env.company_id()? {
            Some(cid) => erp_addon_base::company_calendar::business_date(tx, cid).await?,
            None => env.context_today()?,
        };
        for vals in &mut vals_list {
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let name = next_package_name(env, tx).await?;
                vals.insert("name".into(), Cell::Str(name.into()));
            }
            if matches!(vals.get("pack_date"), None | Some(Cell::Null)) {
                vals.insert("pack_date".into(), Cell::Date(today));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
}

#[methods(StockPicking, addon = "stock")]
impl StockPicking {
    /// Pack all open, unpacked move lines with positive quantity into a new `stock.package`.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_put_in_pack(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            let state_rows = tx
                .query(
                    "SELECT state FROM stock_picking WHERE id = $1 FOR UPDATE",
                    &[&id],
                )
                .await?;
            let state: String = state_rows
                .first()
                .ok_or_else(|| Error::missing("Transfer not found."))?
                .try_get(0)
                .map_err(db_err)?;
            if matches!(state.as_str(), "done" | "cancel") {
                return Err(Error::user(
                    "Cannot put lines in a pack on a completed or cancelled transfer.",
                ));
            }
            let line_ids: Vec<i64> = tx
                .query(
                    "SELECT id::int8 FROM stock_move_line \
                     WHERE picking_id = $1 AND membership = 'open' AND active IS TRUE \
                       AND quantity > 0 AND result_package_id IS NULL \
                     ORDER BY id",
                    &[&id],
                )
                .await?
                .into_iter()
                .map(|r| r.try_get(0))
                .collect::<std::result::Result<Vec<i64>, _>>()
                .map_err(db_err)?;
            if line_ids.is_empty() {
                return Err(Error::user(
                    "There are no unpacked operation lines with quantity to put in a pack.",
                ));
            }
            let env = records.env();
            let pkg = env
                .records("stock.package")?
                .create(tx, vec![RawVals::new()])
                .await?;
            tx.flush().await?;
            let pkg_id = pkg
                .ids()
                .into_iter()
                .next()
                .ok_or_else(|| Error::invariant("Created stock.package returned no id."))?;
            env.records("stock.move.line")?
                .browse(line_ids)?
                .write(
                    tx,
                    RawVals::new().set_cell("result_package_id", Cell::Id(pkg_id)),
                )
                .await?;
            Ok(())
        })
        .await
    }
}
