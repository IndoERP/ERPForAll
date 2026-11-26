// SPDX-License-Identifier: MIT OR Apache-2.0
//! Putaway rules: the table only. Rule selection, capacity checks and the
//! company-change refusal belong to. Observed in.

use erp_sdk::prelude::*;

/// `stock.putaway.rule`: where goods arriving at a location are stored.
#[model(
    name = "stock.putaway.rule",
    description = "Putaway Rule",
    order = "sequence, product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct StockPutawayRule {
    /// Product the rule applies to; empty with an empty category means every product.
    #[field(
        many2one,
        comodel = "product.product",
        ondelete = "cascade",
        index = "btree_not_null",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Product category the rule applies to.
    #[field(
        many2one,
        comodel = "product.category",
        ondelete = "cascade",
        index = "btree_not_null",
        string = "Product Category"
    )]
    pub category_id: Option<RecId>,
    /// Location the goods arrive in.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "cascade",
        index = true,
        check_company = true,
        string = "When product arrives in"
    )]
    pub location_in_id: Option<RecId>,
    /// Location the goods are stored in.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "cascade",
        check_company = true,
        string = "Store to sublocation"
    )]
    pub location_out_id: Option<RecId>,
    /// Priority: lower first.
    #[field(integer, default = 0, string = "Priority")]
    pub sequence: i32,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Package types the rule applies to; empty means any.
    #[field(
        many2many,
        comodel = "stock.package.type",
        relation = "stock_package_type_stock_putaway_rule_rel",
        column1 = "stock_putaway_rule_id",
        column2 = "stock_package_type_id",
        check_company = true,
        string = "Package Type"
    )]
    pub package_type_ids: Vec<RecId>,
    #[field(
        many2one,
        comodel = "stock.storage.category",
        ondelete = "cascade",
        check_company = true,
        string = "Storage Category"
    )]
    pub storage_category_id: Option<RecId>,
    /// Archived rules are ignored.
    #[field(boolean, default = true)]
    pub active: bool,
    /// How a sublocation is chosen below the target.
    #[field(
        selection,
        selection = [
            ("no", "No"),
            ("last_used", "Last Used"),
            ("closest_location", "Closest Location")
        ],
        default = "no"
    )]
    pub sublocation: Option<Arc<str>>,
}

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("A stock putaway rule or storage category could not be read.").with_source(e)
}

/// Check whether `location_id` can accept `incoming_qty` of `product_id` under its configured
/// `stock.storage.category` (if any): `allow_new_product` (`empty`/`same`/`mixed`),
/// per-product capacity, and maximum weight.
pub(crate) async fn location_has_capacity(
    tx: &mut Transaction,
    company_id: i64,
    location_id: i64,
    product_id: i64,
    incoming_qty: Decimal,
) -> Result<bool> {
    let cat_rows = tx
        .query(
            "SELECT sc.id::int8, sc.allow_new_product, COALESCE(sc.max_weight, 0) \
             FROM stock_location l \
             JOIN stock_storage_category sc ON sc.id = l.storage_category_id \
             WHERE l.id = $1",
            &[&location_id],
        )
        .await?;
    let Some(cat) = cat_rows.first() else {
        return Ok(true);
    };
    let cat_id: i64 = cat.try_get(0).map_err(db_err)?;
    let allow_new: String = cat.try_get(1).map_err(db_err)?;
    let max_weight: Decimal = cat.try_get(2).map_err(db_err)?;

    let existing_products = tx
        .query(
            "SELECT product_id::int8, COALESCE(sum(quantity), 0) \
             FROM stock_quant \
             WHERE company_id = $1 AND location_id = $2 AND quantity > 0 \
             GROUP BY product_id",
            &[&company_id, &location_id],
        )
        .await?;
    let mut existing_same_qty = Decimal::ZERO;
    let mut has_any = false;
    let mut has_other = false;
    for row in existing_products {
        let pid: i64 = row.try_get(0).map_err(db_err)?;
        let qty: Decimal = row.try_get(1).map_err(db_err)?;
        if qty > Decimal::ZERO {
            has_any = true;
            if pid == product_id {
                existing_same_qty += qty;
            } else {
                has_other = true;
            }
        }
    }
    match allow_new.as_str() {
        "empty" => {
            if has_any {
                return Ok(false);
            }
        }
        "same" => {
            if has_other {
                return Ok(false);
            }
        }
        "mixed" => {}
        other => {
            return Err(Error::validation(format!(
                "Unknown storage category allow_new_product policy '{other}'."
            )));
        }
    }

    // Per-product quantity capacity limit
    let cap_rows = tx
        .query(
            "SELECT quantity FROM stock_storage_category_capacity \
             WHERE storage_category_id = $1 AND product_id = $2 LIMIT 1",
            &[&cat_id, &product_id],
        )
        .await?;
    if let Some(cap) = cap_rows.first() {
        let limit: Decimal = cap.try_get(0).map_err(db_err)?;
        if existing_same_qty + incoming_qty > limit {
            return Ok(false);
        }
    }

    // Max weight check when configured (> 0)
    if max_weight > Decimal::ZERO {
        let cur_weight: Decimal = tx
            .query(
                "SELECT COALESCE(sum(q.quantity * COALESCE(pt.weight, 0)), 0) \
                 FROM stock_quant q \
                 JOIN product_product pp ON pp.id = q.product_id \
                 JOIN product_template pt ON pt.id = pp.product_tmpl_id \
                 WHERE q.company_id = $1 AND q.location_id = $2 AND q.quantity > 0",
                &[&company_id, &location_id],
            )
            .await?[0]
            .try_get(0)
            .map_err(db_err)?;
        let prod_weight: Decimal = tx
            .query(
                "SELECT COALESCE(pt.weight, 0) FROM product_product pp \
                 JOIN product_template pt ON pt.id = pp.product_tmpl_id WHERE pp.id = $1",
                &[&product_id],
            )
            .await?
            .first()
            .map(|r| r.try_get(0))
            .transpose()
            .map_err(db_err)?
            .unwrap_or(Decimal::ZERO);
        if cur_weight + (incoming_qty * prod_weight) > max_weight {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Resolve the destination sublocation for `(company_id, product_id)` arriving at
/// `location_dest_id` according to active `stock.putaway.rule` and storage category capacity.
pub(crate) async fn resolve_putaway_location(
    tx: &mut Transaction,
    company_id: i64,
    product_id: i64,
    location_dest_id: i64,
    incoming_qty: Decimal,
) -> Result<i64> {
    let cat_rows = tx
        .query(
            "SELECT pt.categ_id::int8 FROM product_product pp \
             JOIN product_template pt ON pt.id = pp.product_tmpl_id WHERE pp.id = $1",
            &[&product_id],
        )
        .await?;
    let categ_id: Option<i64> = cat_rows
        .first()
        .map(|r| r.try_get::<_, Option<i64>>(0))
        .transpose()
        .map_err(db_err)?
        .flatten();
    let rules = tx
        .query(
            "SELECT location_out_id::int8 \
             FROM stock_putaway_rule \
             WHERE active IS TRUE AND company_id = $1 AND location_in_id = $2 \
               AND (product_id = $3 \
                    OR (product_id IS NULL AND category_id = $4) \
                    OR (product_id IS NULL AND category_id IS NULL)) \
             ORDER BY \
               CASE WHEN product_id IS NOT NULL THEN 0 \
                    WHEN category_id IS NOT NULL THEN 1 \
                    ELSE 2 END, \
               sequence, id",
            &[&company_id, &location_dest_id, &product_id, &categ_id],
        )
        .await?;
    for row in rules {
        let candidate: i64 = row.try_get(0).map_err(db_err)?;
        if location_has_capacity(tx, company_id, candidate, product_id, incoming_qty).await? {
            return Ok(candidate);
        }
    }
    Ok(location_dest_id)
}
