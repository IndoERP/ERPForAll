// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock locations, their negative-stock policy row and the removal strategies.
//!
//! The parent store maintains location paths. Stored complete names and nearest active warehouse
//! ownership follow changes through the compute graph. Policy rows and quant-reading
//! usage/archive guards remain with the quant engine and its follow-up slices. Ownership fields
//! cannot be assigned by native or RPC model writes.

use erp_sdk::prelude::*;

use crate::error::StockError;
use crate::warehouse::StockWarehouse;

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
            "Another location of this company already uses this barcode."
        ),
        (
            "_inventory_freq_nonneg",
            "CHECK (cyclic_inventory_frequency >= 0)",
            "The inventory interval of a location cannot be negative."
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
    /// Closest active warehouse rooted at this location or an ancestor. Stored independently
    /// of the reader's permissions; reading its label still uses normal warehouse access.
    #[field(
        many2one,
        comodel = "stock.warehouse",
        compute = "_compute_warehouse_id",
        compute_sudo = true,
        store = true,
        readonly = true,
        copy = false,
        index = true,
        string = "Warehouse"
    )]
    pub warehouse_id: Option<RecId>,
    /// Warehouses rooted exactly here, used to invalidate ownership when their root or
    /// activity changes. This is the inverse of `stock.warehouse.view_location_id`.
    #[field(
        one2many,
        comodel = "stock.warehouse",
        inverse_name = "view_location_id",
        readonly = true
    )]
    pub warehouse_view_ids: Vec<RecId>,
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
    /// Ownership inputs are derived; the insertion trigger creates the policy in the same statement.
    #[method(override)]
    async fn create(
        this: &RecordSet<StockLocation>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        // a `default_<field>` context key or a stored `ir.default` would be filled in by the base
        // create after this check; `default_get` reports both
        let defaulted = !this
            .as_records()
            .default_get(tx, &OWNERSHIP)
            .await?
            .is_empty();
        if defaulted || vals_list.iter().any(has_ownership_assignment) {
            this.as_records()
                .check_access(tx, erp_sdk::Operation::Create)
                .await?;
            return Err(StockError::LocationOwnershipDerived.into());
        }
        let created = sup.call(tx, this.as_records(), (vals_list,)).await?;
        crate::res_company::invalidate_company_locations(tx).await?;
        Ok(created)
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<StockLocation>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        crate::res_company::invalidate_company_locations(tx).await
    }

    /// Nearest-root ownership propagates through the existing recursive compute graph.
    /// Parent-store notifications update only moved paths; root membership and activity
    /// notifications reach their descendants through `location_id.warehouse_id`.
    #[compute(depends = [
        "parent_path",
        "location_id.warehouse_id",
        "warehouse_view_ids.active",
        "warehouse_view_ids.sequence"
    ])]
    async fn _compute_warehouse_id(
        this: &RecordSet<StockLocation>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockLocation>,
    ) -> Result<()> {
        // Always load archived roots too and test their flag explicitly: the stored result
        // must not depend on the initiating caller's active_test context.
        let all = this.with_context([("active_test", Value::Bool(false))]);
        let roots = all.mapped(tx, StockLocation::WAREHOUSE_VIEW_IDS).await?;
        let warehouses = RecordSet::<StockWarehouse>::try_from(
            all.env()
                .records("stock.warehouse")?
                .browse_ids(roots.iter().flatten().copied()),
        )?;
        let active = warehouses.mapped(tx, StockWarehouse::ACTIVE).await?;
        let sequences = warehouses.mapped(tx, StockWarehouse::SEQUENCE).await?;
        let candidates: std::collections::BTreeMap<_, _> = warehouses
            .rec_ids()
            .iter()
            .copied()
            .zip(active.into_iter().zip(sequences))
            .collect();
        let parents = all.mapped(tx, StockLocation::LOCATION_ID).await?;
        let parent_records = RecordSet::<StockLocation>::try_from(
            all.as_records()
                .browse_ids(parents.iter().flatten().copied()),
        )?;
        let parent_values = parent_records
            .mapped(tx, StockLocation::WAREHOUSE_ID)
            .await?;
        let inherited: std::collections::BTreeMap<_, _> = parent_records
            .rec_ids()
            .iter()
            .copied()
            .zip(parent_values)
            .collect();
        for (index, (root_ids, parent)) in roots.into_iter().zip(parents).enumerate() {
            let mut direct = None;
            for id in root_ids {
                let (is_active, sequence) = candidates.get(&id).ok_or_else(|| {
                    Error::internal("location ownership is missing a warehouse root")
                })?;
                if *is_active {
                    // For several warehouses sharing one root, preserve the source's last
                    // entry in warehouse order (sequence, id); depth still wins over sequence.
                    let candidate = (*sequence, id);
                    if direct.is_none_or(|current| candidate > current) {
                        direct = Some(candidate);
                    }
                }
            }
            let owner = match direct {
                Some((_, id)) => Some(id),
                None => match parent {
                    Some(id) => *inherited.get(&id).ok_or_else(|| {
                        Error::internal("location ownership is missing a parent value")
                    })?,
                    None => None, // a root outside a warehouse has no warehouse
                },
            };
            out.set(index, StockLocation::WAREHOUSE_ID, owner)?;
        }
        Ok(())
    }

    /// `complete_name`: the parent's full name, `/`, then the name; a root or a view
    /// shows its own name (/068). Use the graph's prefetched recursive input
    /// rather than issuing an async field read while this compute group is protected.
    #[compute(depends = ["name", "usage", "location_id.complete_name"])]
    fn _compute_complete_name(batch: &mut Batch<'_, StockLocation>) -> Result<()> {
        for index in batch.indices() {
            let name = batch.get(index, StockLocation::NAME)?;
            let usage = batch.get(index, StockLocation::USAGE)?;
            let parent = batch.get(index, StockLocation::LOCATION_ID)?;
            let prefix = match parent {
                Some(id) if usage.as_deref() != Some("view") => Some(
                    batch
                        .value_of(StockLocation::COMPLETE_NAME, id)?
                        .ok_or_else(|| {
                            Error::internal("a parent location has no stored complete name")
                        })?,
                ),
                Some(_) | None => None, // view locations and roots show only their own name
            };
            // A missing own name stays missing; the required-name check refuses it.
            let value: Option<Arc<str>> = name.map(|own| match prefix {
                Some(prefix) => format!("{prefix}/{own}").into(),
                None => own,
            });
            batch.set(index, StockLocation::COMPLETE_NAME, value)?;
        }
        Ok(())
    }
}

/// The ownership fields are outputs of topology, never an alternate way to edit warehouse roots.
const OWNERSHIP: [&str; 2] = ["warehouse_id", "warehouse_view_ids"];
fn has_ownership_assignment(vals: &RawVals) -> bool {
    OWNERSHIP
        .iter()
        .any(|field| vals.contains(field) || vals.iter_commands().any(|(name, _)| name == *field))
}

/// Keep this in the existing location write chain alongside its quant guards.
pub(crate) async fn check_ownership_assignment(
    this: &RecordSet<StockLocation>,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    if !this.is_empty() && has_ownership_assignment(vals) {
        this.as_records()
            .check_access(tx, erp_sdk::Operation::Write)
            .await?;
        for field in ["warehouse_id", "warehouse_view_ids"] {
            if (vals.contains(field) || vals.iter_commands().any(|(name, _)| name == field))
                && !tx
                    .has_field_access(this.env(), "stock.location", field, FieldOp::Write)
                    .await?
            {
                return Err(Error::access(format!(
                    "Writing stock.location.{field} is not allowed."
                )));
            }
        }
        return Err(StockError::LocationOwnershipDerived.into());
    }
    Ok(())
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
    /// Refuse a guard enabled explicitly or by default (a context key or a stored default) before
    /// the policy unique key can mask the typed unavailable-feature error. Malformed values remain
    /// the base create path's responsibility; the constraint below also checks its normalized result.
    #[method(override)]
    async fn create(
        this: &RecordSet<StockLocationPolicy>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, erp_sdk::Operation::Create)
            .await?;
        // a row without the flag takes its default (context key, stored default or false)
        let defaulted = matches!(
            this.as_records()
                .default_get(tx, &["forbid_negative"])
                .await?
                .get("forbid_negative"),
            Some(Value::Bool(true))
        );
        for vals in &vals_list {
            let enabled = match vals.get("forbid_negative") {
                Some(cell) => matches!(cell, Cell::Bool(true)),
                None => defaulted,
            };
            if enabled
                && let Some(Cell::Id(location_id)) = vals.get("location_id")
                && *location_id > 0
            {
                let location = this
                    .env()
                    .records("stock.location")?
                    .browse([*location_id])?;
                location.check_access(tx, erp_sdk::Operation::Read).await?;
                return Err(StockError::ForbidNegativeUnavailable {
                    location_id: *location_id,
                }
                .into());
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

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
