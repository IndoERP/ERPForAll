// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock locations, their negative-stock policy row and the removal strategies.
//!
//! Schema only: the location tree, its stored complete name and the one policy row per location
//! (1, amendment B3). Usage guards, warehouse ownership and the negative-stock guard
//! belong to later slices.

use erp_sdk::prelude::*;

use crate::error::StockError;

/// `stock.location`: a node of the location tree, physical (internal, transit) or virtual.
#[model(
    name = "stock.location",
    description = "Inventory Locations",
    parent_store = true,
    parent_name = "location_id",
    order = "complete_name, id",
    rec_name = "complete_name",
    rec_names_search = ["complete_name", "barcode"],
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    company_match = "parent_of",
    constraints = [
        (
            "_barcode_company_uniq",
            "UNIQUE (barcode, company_id)",
            "The barcode for a location must be unique per company!"
        ),
        (
            "_inventory_freq_nonneg",
            "CHECK (cyclic_inventory_frequency >= 0)",
            "The inventory frequency (days) for a location must be non-negative"
        ),
        (
            "_generated_role_complete",
            "CHECK ((generated_warehouse_id IS NULL) = (generated_addon IS NULL) AND (generated_addon IS NULL) = (generated_role IS NULL))",
            "A generated location names its warehouse, addon and role together."
        ),
        (
            "_generated_role_uniq",
            "UNIQUE (generated_warehouse_id, generated_addon, generated_role)",
            "A warehouse has one location per addon role."
        ),
    ],
    indexes = [("_parent_path_id_idx", "(parent_path, id)")],
)]
pub struct StockLocation {
    /// Location name.
    #[field(char, required = true, string = "Location Name")]
    pub name: Option<Arc<str>>,
    /// Parent path joined with `/`; a view location shows only its own name.
    #[field(
        char,
        compute = "_compute_complete_name",
        store = true,
        string = "Full Location Name"
    )]
    pub complete_name: Option<Arc<str>>,
    /// Archived locations stay readable for history.
    #[field(boolean, default = true)]
    pub active: bool,
    /// What the location stands for.
    #[field(
        selection,
        selection = [
            ("supplier", "Vendor"),
            ("view", "Virtual"),
            ("internal", "Internal"),
            ("customer", "Customer"),
            ("inventory", "Inventory Loss"),
            ("production", "Production"),
            ("transit", "Transit")
        ],
        default = "internal",
        required = true,
        index = true,
        string = "Location Type"
    )]
    pub usage: Option<Arc<str>>,
    /// Parent location.
    #[field(
        many2one,
        comodel = "stock.location",
        index = true,
        string = "Parent Location"
    )]
    pub location_id: Option<RecId>,
    /// Direct children.
    #[field(one2many, comodel = "stock.location", inverse_name = "location_id")]
    pub child_ids: Vec<RecId>,
    /// Delimiter-terminated ancestry maintained by the parent store (topology ADR D8).
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
    /// Owning company; empty for a location shared between companies.
    #[field(many2one, comodel = "res.company", index = true)]
    pub company_id: Option<RecId>,
    /// Suggests replenishment for this location; its computed default comes with.
    #[field(boolean, default = false, copy = false, string = "Replenishments")]
    pub replenish_location: bool,
    /// Default removal strategy of this location.
    #[field(many2one, comodel = "product.removal", string = "Removal Strategy")]
    pub removal_strategy_id: Option<RecId>,
    /// Barcode, unique per company.
    #[field(char, copy = false)]
    pub barcode: Option<Arc<str>>,
    /// Days between counts; 0 means no cyclic count.
    #[field(integer, default = 0, string = "Inventory Frequency")]
    pub cyclic_inventory_frequency: i32,
    /// Date of the last applied count.
    #[field(date, readonly = true, string = "Last Inventory")]
    pub last_inventory_date: Option<Date>,
    /// Storage category limiting what the location holds.
    #[field(
        many2one,
        comodel = "stock.storage.category",
        index = "btree_not_null",
        string = "Storage Category"
    )]
    pub storage_category_id: Option<RecId>,
    /// The negative-stock policy row of this location (exactly one).
    #[field(
        one2many,
        comodel = "stock.location.policy",
        inverse_name = "location_id"
    )]
    pub policy_ids: Vec<RecId>,
    /// Warehouse whose generated role this location fills (topology ADR D9).
    #[field(
        many2one,
        comodel = "stock.warehouse",
        ondelete = "restrict",
        copy = false,
        string = "Generated For Warehouse"
    )]
    pub generated_warehouse_id: Option<RecId>,
    /// Addon that owns the generated role.
    #[field(char, copy = false)]
    pub generated_addon: Option<Arc<str>>,
    /// Role name within the warehouse and addon.
    #[field(char, copy = false)]
    pub generated_role: Option<Arc<str>>,
}

#[methods(StockLocation, addon = "stock")]
impl StockLocation {
    /// Every location gets its policy row in the statement batch that creates it (B3).
    /// Inserts outside the ORM get none until the insert trigger.
    #[method(override)]
    async fn create(
        this: &RecordSet<StockLocation>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        let rows = created
            .ids()
            .into_iter()
            .map(|id| RawVals::new().set_cell("location_id", Cell::Id(id)))
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            this.env()
                .records("stock.location.policy")?
                .create(tx, rows)
                .await?;
        }
        Ok(created)
    }

    /// `complete_name`: the parent's complete name, `/`, then the name; a root or a view
    /// location shows its own name. One read of all parents.
    #[compute(depends = ["name", "usage", "location_id.complete_name"])]
    async fn _compute_complete_name(
        this: &RecordSet<StockLocation>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockLocation>,
    ) -> Result<()> {
        let names = this.mapped(tx, StockLocation::NAME).await?;
        let usages = this.mapped(tx, StockLocation::USAGE).await?;
        let parents = this.mapped(tx, StockLocation::LOCATION_ID).await?;
        let with_parent: Vec<RecId> = parents
            .iter()
            .zip(&usages)
            .filter(|(_, usage)| usage.as_deref() != Some("view"))
            .filter_map(|(parent, _)| *parent)
            .collect();
        let parent_set =
            RecordSet::<StockLocation>::try_from(this.as_records().browse_ids(with_parent))?;
        let mut parent_names = parent_set
            .mapped(tx, StockLocation::COMPLETE_NAME)
            .await?
            .into_iter();
        for (index, ((name, usage), parent)) in
            names.into_iter().zip(usages).zip(parents).enumerate()
        {
            let prefix = if parent.is_some() && usage.as_deref() != Some("view") {
                let parent_name = parent_names.next().ok_or_else(|| {
                    Error::internal("location parent names are shorter than the parents read")
                })?;
                Some(parent_name.ok_or_else(|| {
                    Error::internal("a parent location has no stored complete name")
                })?)
            } else {
                None
            };
            // no name yet: no complete name; the required check refuses the row itself
            let value = name.map(|own| match &prefix {
                Some(prefix) => format!("{prefix}/{own}").into(),
                None => own,
            });
            out.set(index, StockLocation::COMPLETE_NAME, value)?;
        }
        Ok(())
    }
}

/// `stock.location.policy`: the negative-stock setting of one location, on its own narrow row so
/// that changing it never locks the location row (1, D6.7).
#[model(
    name = "stock.location.policy",
    description = "Location Stock Policy",
    company_access = parent(edge = "location_id"),
    constraints = [(
        "_location_uniq",
        "UNIQUE (location_id)",
        "A location has one stock policy."
    )],
)]
pub struct StockLocationPolicy {
    /// The location this policy belongs to; deleting the location deletes it.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "cascade",
        string = "Location"
    )]
    pub location_id: Option<RecId>,
    /// Refuse stock operations that would make a key at this exact location negative.
    #[field(boolean, default = false, string = "Forbid Negative Stock")]
    pub forbid_negative: bool,
}

#[methods(StockLocationPolicy, addon = "stock")]
impl StockLocationPolicy {
    /// Until the negative-stock guard exists, switching it on is refused (B3).
    #[constrains("forbid_negative")]
    async fn _check_forbid_negative_unavailable(
        this: &RecordSet<StockLocationPolicy>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let flags = this
            .mapped(tx, StockLocationPolicy::FORBID_NEGATIVE)
            .await?;
        let locations = this.mapped(tx, StockLocationPolicy::LOCATION_ID).await?;
        for (flag, location) in flags.into_iter().zip(locations) {
            if flag {
                let location = location.ok_or_else(|| {
                    Error::internal("a stock location policy row has no location")
                })?;
                return Err(StockError::ForbidNegativeUnavailable {
                    location_id: location.get(),
                }
                .into());
            }
        }
        Ok(())
    }
}

/// `product.removal`: a named removal strategy.
#[model(name = "product.removal", description = "Removal Strategy")]
pub struct ProductRemoval {
    /// Display name.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// Strategy key, for example `fifo`.
    #[field(char, required = true)]
    pub method: Option<Arc<str>>,
}
