// SPDX-License-Identifier: MIT OR Apache-2.0
//! Delivered Sales quantities come from the private direct-shipment association.

use std::collections::BTreeMap;

use erp_addon_sale::SaleOrderLine;
use erp_addon_stock::stock_move_line::StockMoveLine;
use erp_sdk::Operation;
use erp_sdk::migrations::SemverVersion;
use erp_sdk::prelude::*;

#[extend(SaleOrderLine, addon = "sale_stock")]
struct DeliveredLine {
    #[field(selection, selection_add = [("stock_move", "Stock Moves")], ondelete = [("stock_move", "set default")])]
    qty_delivered_method: Option<Arc<str>>,
}

fn decode(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Reading the sales delivery quantities failed.").with_source(error)
}

async fn linked(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<i64>> {
    let rows = tx
        .query(
            "SELECT sale_line_id FROM sale_stock_delivery_move_link
             WHERE sale_line_id=ANY($1) ORDER BY sale_line_id",
            &[&ids],
        )
        .await?;
    rows.iter()
        .map(|row| row.try_get(0).map_err(decode))
        .collect()
}

fn quantity_query() -> Result<String> {
    let convert =
        erp_addon_uom::stock::product_quantity_sql("qty", "source_factor", "dest_factor", "digits")
            .map_err(|error| {
                Error::invariant("Cannot convert sales delivery units.").with_source(error)
            })?;
    Ok(format!(
        "WITH sources AS (
             SELECT l.id, l.product_id, l.product_uom_id AS unit, u.factor AS target_factor,
                    link.company_id, root.id AS move, root.picking_id AS picking
             FROM sale_stock_delivery_move_link link
             JOIN sale_order_line l ON l.id=link.sale_line_id AND l.company_id=link.company_id
             JOIN sale_stock_delivery_link header ON header.order_id=l.order_id AND header.company_id=l.company_id
             JOIN stock_move root ON root.id=link.move_id AND root.company_id=link.company_id AND root.picking_id=header.picking_id
             JOIN uom_uom u ON u.id=l.product_uom_id
             WHERE l.id=ANY($1)
         ), shipments AS (
             SELECT s.id,s.product_id,s.unit,s.target_factor,s.company_id,m.id AS move,m.picking_id AS picking
             FROM sources s JOIN stock_move m ON (m.id=s.move OR m.backorder_root_move_id=s.move)
                 AND m.company_id=s.company_id AND m.product_id=s.product_id
         ), actual AS (
             SELECT s.id,s.target_factor AS dest_factor,m.quantity AS qty,m.uom_factor AS source_factor,
                    m.product_unit_digits AS digits,m.uom_id=s.unit AS same_unit,
                    CASE WHEN m.id=s.move THEN 1 ELSE -1 END AS sign
             FROM shipments s
             JOIN stock_move m ON (m.id=s.move OR m.origin_returned_move_id=s.move)
                              AND m.state='done' AND m.is_scrap IS NOT TRUE
                              AND m.product_id=s.product_id AND m.company_id=s.company_id
             JOIN stock_picking p ON p.id=m.picking_id AND p.company_id=s.company_id
             JOIN stock_location src ON src.id=m.location_id
             JOIN stock_location dst ON dst.id=m.location_dest_id
             WHERE (m.id=s.move AND src.usage='internal' AND dst.usage='customer')
                OR (m.origin_returned_move_id=s.move AND p.return_id=s.picking
                    AND src.usage='customer' AND dst.usage='internal')
         )
         SELECT s.id,COALESCE(SUM(a.sign * (CASE WHEN a.same_unit THEN a.qty ELSE {convert} END)),0)::numeric AS delivered
         FROM sources s LEFT JOIN actual a ON a.id=s.id GROUP BY s.id ORDER BY s.id"
    ))
}

async fn quantities(tx: &mut Transaction, ids: &[i64]) -> Result<BTreeMap<RecId, Decimal>> {
    let rows = tx.query(&quantity_query()?, &[&ids]).await?;
    rows.iter()
        .map(|row| {
            let id = RecId::from_real(row.try_get(0).map_err(decode)?)
                .ok_or_else(|| Error::invariant("Invalid linked sales line."))?;
            Ok((id, row.try_get(1).map_err(decode)?))
        })
        .collect()
}

async fn lock_lines(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    tx.query(
        "SELECT id FROM sale_order_line WHERE id=ANY($1) ORDER BY id FOR UPDATE",
        &[&ids],
    )
    .await?;
    Ok(())
}

pub(super) async fn refresh(tx: &mut Transaction, lines: &Records) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let ids = lines.ids();
    lock_lines(tx, &ids).await?;
    let projection = quantity_query()?;
    let update = format!(
        "UPDATE sale_order_line l SET qty_delivered=v.delivered, qty_delivered_method='stock_move'
         FROM ({projection}) v
         WHERE l.id=v.id AND (l.qty_delivered,l.qty_delivered_method) IS DISTINCT FROM (v.delivered,'stock_move')
         RETURNING l.id"
    );
    let changed = tx.query(&update, &[&ids]).await?;
    let changed = changed
        .iter()
        .map(|row| row.try_get(0).map_err(decode))
        .collect::<Result<Vec<i64>>>()?;
    if !changed.is_empty() {
        lines
            .browse(changed)?
            .modified_external(tx, &["qty_delivered", "qty_delivered_method"])
            .await?;
    }
    Ok(())
}

#[methods(SaleOrderLine, addon = "sale_stock")]
impl DeliveredLine {
    #[method(override)]
    async fn _prepare_qty_delivered(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<BTreeMap<RecId, Decimal>> {
        let lines = this.as_records();
        let mut totals: BTreeMap<RecId, Decimal> = sup.call(tx, lines, ()).await?;
        totals.extend(quantities(tx, &lines.ids()).await?);
        Ok(totals)
    }

    #[method(override)]
    async fn _get_consu_qty_delivered_method(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<BTreeMap<RecId, String>> {
        let lines = this.as_records();
        let mut methods: BTreeMap<RecId, String> = sup.call(tx, lines, ()).await?;
        for id in linked(tx, &lines.ids()).await? {
            let key = RecId::from_real(id)
                .ok_or_else(|| Error::invariant("Invalid linked sales line."))?;
            methods.insert(key, "stock_move".into());
        }
        Ok(methods)
    }

    #[method(override)]
    async fn _stock_moves_all_finished(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<std::collections::BTreeSet<RecId>> {
        let lines = this.as_records();
        let mut finished: std::collections::BTreeSet<RecId> = sup.call(tx, lines, ()).await?;
        let ids = lines.ids();
        if ids.is_empty() {
            return Ok(finished);
        }
        let rows = tx
            .query(
                "WITH shipments AS ( \
                     SELECT link.sale_line_id, m.state::text AS state \
                     FROM sale_stock_delivery_move_link link \
                     JOIN stock_move root ON root.id = link.move_id AND root.company_id = link.company_id \
                     JOIN stock_move m ON (m.id = root.id OR m.backorder_root_move_id = root.id) \
                       AND m.company_id = link.company_id \
                     WHERE link.sale_line_id = ANY($1::int8[]) \
                 ) \
                 SELECT sale_line_id::int8 \
                 FROM shipments \
                 GROUP BY sale_line_id \
                 HAVING bool_and(state IN ('done', 'cancel')) AND bool_or(state = 'done')",
                &[&ids],
            )
            .await?;
        for row in rows {
            let id: i64 = row.try_get(0).map_err(decode)?;
            let rec_id =
                RecId::from_real(id).ok_or_else(|| Error::invariant("Invalid linked sales line."))?;
            finished.insert(rec_id);
        }
        Ok(finished)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let lines = this.as_records();
        if let Some(value) = vals.get("qty_delivered") {
            lines.check_access(tx, Operation::Write).await?;
            let ids = lines.ids();
            // Serialize the comparison and the following write with stock completion.
            lock_lines(tx, &ids).await?;
            let measured = quantities(tx, &ids).await?;
            // Stored compute writes may repeat the authoritative result. A caller can
            // likewise send the unchanged display value with an unrelated line edit.
            if measured
                .values()
                .any(|quantity| value != &Cell::Decimal(*quantity))
            {
                return Err(Error::validation(
                    "Validate the stock transfer to change delivered quantities on a linked sales line.",
                ));
            }
        }
        if !vals.contains("product_uom_id") && !vals.contains("product_id") {
            return sup.call(tx, lines, (vals,)).await;
        }
        tx.savepoint(async |tx| {
            let () = sup.call(tx, lines, (vals,)).await?;
            refresh(tx, lines).await
        })
        .await
    }
}

#[extend(StockMoveLine, addon = "sale_stock")]
struct OutboundCompletion {}

#[methods(StockMoveLine, addon = "sale_stock")]
impl OutboundCompletion {
    #[method(override)]
    async fn _after_stock_completion(
        this: &RecordSet<StockMoveLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        let () = sup.call(tx, records, ()).await?;
        let rows = tx
            .query(
                "SELECT DISTINCT link.sale_line_id FROM stock_move_line item
             JOIN stock_move m ON m.id=item.move_id AND m.company_id=item.company_id
             LEFT JOIN stock_move returned ON returned.id=m.origin_returned_move_id AND returned.company_id=m.company_id
             JOIN sale_stock_delivery_move_link link ON link.company_id=m.company_id
                 AND link.move_id=COALESCE(returned.backorder_root_move_id,returned.id,m.backorder_root_move_id,m.id)
             WHERE item.id=ANY($1) AND item.membership='done' ORDER BY link.sale_line_id",
                &[&records.ids()],
            )
            .await?;
        let ids = rows
            .iter()
            .map(|row| row.try_get(0).map_err(decode))
            .collect::<Result<Vec<i64>>>()?;
        refresh(tx, &records.env().records("sale.order.line")?.browse(ids)?).await
    }
}

#[erp_sdk::migration(addon="sale_stock",version="1.1",stage=post)]
async fn backfill_delivered(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    let mut cursor = 0_i64;
    let lines = tx.env().records("sale.order.line")?;
    loop {
        let rows = tx
            .query(
                "SELECT sale_line_id FROM sale_stock_delivery_move_link
                 WHERE sale_line_id>$1 ORDER BY sale_line_id LIMIT 512",
                &[&cursor],
            )
            .await?;
        let ids = rows
            .iter()
            .map(|row| row.try_get(0).map_err(decode))
            .collect::<Result<Vec<i64>>>()?;
        let Some(&last) = ids.last() else {
            return Ok(());
        };
        refresh(tx, &lines.browse(ids)?).await?;
        cursor = last;
    }
}
