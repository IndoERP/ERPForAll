// SPDX-License-Identifier: MIT OR Apache-2.0
//! Procurement and multi-step route chaining:
//! - Make-to-order (`make_to_order` / `mts_else_mto`) upstream move & transfer generation
//! - `stock.replenish_on_order` Settings toggle for `stock.route_warehouse0_mto`
//! - Multi-step reception/delivery route synchronization on `stock.warehouse`
//! - Downstream move readiness & push-rule transfer generation upon completion.

use std::collections::BTreeSet;

use erp_sdk::prelude::*;

use crate::error::StockError;

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("A stock procurement or route record could not be read.").with_source(e)
}

const MAX_MTO_DEPTH: usize = 16;

/// Toggle the built-in `Replenish on Order (MTO)` pull rules for `company_id` without disabling
/// the shared global route for other companies.
pub(crate) async fn toggle_mto_route(
    env: &Env,
    tx: &mut Transaction,
    company_id: i64,
    enabled: bool,
) -> Result<()> {
    let sudo = env.sudo(true);
    let mto_rows = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data \
             WHERE module = 'stock' AND name = 'route_warehouse0_mto' AND model = 'stock.route'",
            &[],
        )
        .await?;
    let Some(row) = mto_rows.first() else {
        return Ok(());
    };
    let route_id: i64 = row.try_get(0).map_err(db_err)?;

    if !enabled {
        // Deactivate only this company's MTO rules; keep the shared route active if other
        // companies still have active MTO rules.
        tx.execute(
            "UPDATE stock_rule SET active = false WHERE route_id = $1 AND company_id = $2",
            &[&route_id, &company_id],
        )
        .await?;
        let remaining: i64 = tx
            .query(
                "SELECT count(*)::int8 FROM stock_rule WHERE route_id = $1 AND active IS TRUE",
                &[&route_id],
            )
            .await?
            .first()
            .ok_or_else(|| Error::internal("MTO rule count returned no row."))?
            .try_get(0)
            .map_err(db_err)?;
        if remaining == 0 {
            sudo.records("stock.route")?
                .browse([route_id])?
                .write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                .await?;
        }
        tx.flush().await?;
        return Ok(());
    }

    sudo.records("stock.route")?
        .browse([route_id])?
        .write(tx, RawVals::new().set_cell("active", Cell::Bool(true)))
        .await?;
    tx.execute(
        "UPDATE stock_rule SET active = true WHERE route_id = $1 AND company_id = $2",
        &[&route_id, &company_id],
    )
    .await?;
    // Ensure each active warehouse of this company has an MTO pull rule from Vendors -> Stock
    // so confirming an MTO outgoing move can procure stock without manual rule authoring.
    let vendor_rows = tx
        .query(
            "SELECT id::int8 FROM stock_location WHERE usage = 'supplier' AND active IS TRUE ORDER BY id LIMIT 1",
            &[],
        )
        .await?;
    let Some(vendor_row) = vendor_rows.first() else {
        return Ok(());
    };
    let vendor_loc: i64 = vendor_row.try_get(0).map_err(db_err)?;
    let warehouses = tx
        .query(
            "SELECT w.id::int8, w.name, w.lot_stock_id::int8, w.in_type_id::int8 \
             FROM stock_warehouse w \
             WHERE w.company_id = $1 AND w.active IS TRUE ORDER BY w.id",
            &[&company_id],
        )
        .await?;
    let rule_model = sudo.records("stock.rule")?;
    for wh in warehouses {
        let wh_id: i64 = wh.try_get(0).map_err(db_err)?;
        let wh_name: String = wh.try_get(1).map_err(db_err)?;
        let stock_loc: i64 = wh.try_get(2).map_err(db_err)?;
        let in_type: Option<i64> = wh.try_get(3).map_err(db_err)?;
        let Some(in_type_id) = in_type else {
            continue;
        };
        let exists: i64 = tx
            .query(
                "SELECT count(*)::int8 FROM stock_rule \
                 WHERE route_id = $1 AND company_id = $2 AND location_dest_id = $3 \
                   AND action IN ('pull', 'pull_push') AND active IS TRUE",
                &[&route_id, &company_id, &stock_loc],
            )
            .await?[0]
            .try_get(0)
            .map_err(db_err)?;
        if exists == 0 {
            let mut vals = RawVals::new();
            vals.insert(
                "name".into(),
                Cell::Str(format!("{wh_name}: Vendors → Stock (MTO)").into()),
            );
            vals.insert("action".into(), Cell::Str("pull".into()));
            vals.insert("procure_method".into(), Cell::Str("make_to_stock".into()));
            vals.insert("location_src_id".into(), Cell::Id(vendor_loc));
            vals.insert("location_dest_id".into(), Cell::Id(stock_loc));
            vals.insert("picking_type_id".into(), Cell::Id(in_type_id));
            vals.insert("route_id".into(), Cell::Id(route_id));
            vals.insert("warehouse_id".into(), Cell::Id(wh_id));
            vals.insert("company_id".into(), Cell::Id(company_id));
            vals.insert("sequence".into(), Cell::Int(10));
            rule_model.create(tx, vec![vals]).await?;
        }
    }
    tx.flush().await?;
    Ok(())
}

/// Create upstream procurement moves and transfers for draft MTO moves that have no origin yet.
/// Returns the IDs of newly created upstream moves (to be confirmed alongside the callers), or
/// `StockError::NeedsProcurement` when an MTO move has no matching active pull rule.
pub(crate) async fn procure_mto_moves(
    records: &Records,
    tx: &mut Transaction,
    moves: &[i64],
    visited: &mut BTreeSet<(i64, i64, i64)>,
    depth: usize,
) -> Result<Vec<i64>> {
    if moves.is_empty() {
        return Ok(Vec::new());
    }
    if depth >= MAX_MTO_DEPTH {
        return Err(Error::validation(
            "Make-to-order procurement exceeded maximum chain depth (possible cyclic stock rules).",
        ));
    }
    let candidates = tx
        .query(
            "SELECT m.id::int8, m.company_id::int8, m.product_id::int8, m.product_uom_qty, \
                    m.uom_id::int8, m.location_id::int8, m.origin, m.partner_id::int8, \
                    m.procure_method, COALESCE(r.procure_method, '') \
             FROM stock_move m \
             LEFT JOIN stock_rule r ON r.id = m.rule_id \
             WHERE m.id = ANY($1) AND m.state = 'draft' \
               AND NOT EXISTS (SELECT 1 FROM stock_move_move_rel o WHERE o.move_dest_id = m.id) \
               AND (m.procure_method = 'make_to_order' OR r.procure_method = 'mts_else_mto') \
             ORDER BY m.id",
            &[&moves],
        )
        .await?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let env = records.env();
    let picking_model = env.records("stock.picking")?;
    let move_model = env.records("stock.move")?;
    let mut created_upstream = Vec::new();

    for row in candidates {
        let dest_move_id: i64 = row.try_get(0).map_err(db_err)?;
        let company_id: i64 = row.try_get(1).map_err(db_err)?;
        let product_id: i64 = row.try_get(2).map_err(db_err)?;
        let mut demand: Decimal = row.try_get(3).map_err(db_err)?;
        let uom_id: i64 = row.try_get(4).map_err(db_err)?;
        let needed_loc: i64 = row.try_get(5).map_err(db_err)?;
        let origin: Option<String> = row.try_get(6).map_err(db_err)?;
        let partner_id: Option<i64> = row.try_get(7).map_err(db_err)?;
        let move_procure: String = row.try_get(8).map_err(db_err)?;
        let rule_procure_hint: String = row.try_get(9).map_err(db_err)?;

        // For mts_else_mto, check free stock first and only procure the shortfall.
        if move_procure != "make_to_order" && rule_procure_hint == "mts_else_mto" {
            let free: Decimal = tx
                .query(
                    "SELECT COALESCE(SUM(quantity - reserved_quantity), 0) FROM stock_quant \
                     WHERE company_id = $1 AND product_id = $2 AND location_id = $3 \
                       AND quantity > reserved_quantity",
                    &[&company_id, &product_id, &needed_loc],
                )
                .await?
                .first()
                .ok_or_else(|| Error::internal("Free stock check returned no row."))?
                .try_get(0)
                .map_err(db_err)?;
            if free >= demand {
                continue;
            }
            if free > Decimal::ZERO {
                demand -= free;
            }
        }

        if !visited.insert((company_id, product_id, needed_loc)) {
            return Err(Error::validation(
                "Cycle detected in make-to-order stock pull rules.",
            ));
        }

        // Find the highest-priority active pull/pull_push rule supplying `needed_loc`,
        // preferring routes explicitly assigned to the move or product template.
        let rule_rows = tx
            .query(
                "SELECT sr.id::int8, sr.location_src_id::int8, sr.picking_type_id::int8, sr.procure_method \
                 FROM stock_rule sr \
                 JOIN stock_route rt ON rt.id = sr.route_id \
                 WHERE sr.active IS TRUE AND rt.active IS TRUE \
                   AND sr.action IN ('pull', 'pull_push') \
                   AND sr.location_dest_id = $1 \
                   AND (sr.company_id = $2 OR sr.company_id IS NULL) \
                 ORDER BY \
                   CASE WHEN EXISTS ( \
                     SELECT 1 FROM stock_route_move rm \
                     WHERE rm.move_id = $3 AND rm.route_id = rt.id \
                   ) THEN 0 \
                   WHEN EXISTS ( \
                     SELECT 1 FROM stock_route_product rp \
                     JOIN product_product pp ON pp.product_tmpl_id = rp.product_id \
                     WHERE pp.id = $4 AND rp.route_id = rt.id \
                   ) THEN 1 \
                   ELSE 2 END, \
                   rt.sequence, sr.sequence, sr.id LIMIT 1",
                &[&needed_loc, &company_id, &dest_move_id, &product_id],
            )
            .await?;
        let Some(rule) = rule_rows.first() else {
            return Err(StockError::NeedsProcurement.into());
        };
        let rule_id: i64 = rule.try_get(0).map_err(db_err)?;
        let src_loc: i64 = rule.try_get(1).map_err(db_err)?;
        let picking_type_id: i64 = rule.try_get(2).map_err(db_err)?;
        let rule_procure: String = rule.try_get(3).map_err(db_err)?;

        let mut pick_vals = RawVals::new();
        pick_vals.insert("company_id".into(), Cell::Id(company_id));
        pick_vals.insert("picking_type_id".into(), Cell::Id(picking_type_id));
        pick_vals.insert("location_id".into(), Cell::Id(src_loc));
        pick_vals.insert("location_dest_id".into(), Cell::Id(needed_loc));
        if let Some(ref orig) = origin {
            pick_vals.insert("origin".into(), Cell::Str(orig.as_str().into()));
        }
        if let Some(pid) = partner_id {
            pick_vals.insert("partner_id".into(), Cell::Id(pid));
        }
        let created_pick = picking_model.create(tx, vec![pick_vals]).await?;
        tx.flush().await?;
        let pick_id = created_pick
            .ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::invariant("Created procurement transfer returned no id."))?;

        let mut mv_vals = RawVals::new();
        mv_vals.insert("company_id".into(), Cell::Id(company_id));
        mv_vals.insert("picking_id".into(), Cell::Id(pick_id));
        mv_vals.insert("picking_type_id".into(), Cell::Id(picking_type_id));
        mv_vals.insert("product_id".into(), Cell::Id(product_id));
        mv_vals.insert("product_uom_qty".into(), Cell::Decimal(demand));
        mv_vals.insert("uom_id".into(), Cell::Id(uom_id));
        mv_vals.insert("location_id".into(), Cell::Id(src_loc));
        mv_vals.insert("location_dest_id".into(), Cell::Id(needed_loc));
        mv_vals.insert("rule_id".into(), Cell::Id(rule_id));
        let upstream_method = if rule_procure == "make_to_order" {
            "make_to_order"
        } else {
            "make_to_stock"
        };
        mv_vals.insert("procure_method".into(), Cell::Str(upstream_method.into()));
        if let Some(orig) = origin {
            mv_vals.insert("origin".into(), Cell::Str(orig.into()));
        }
        let created_mv = move_model.create(tx, vec![mv_vals]).await?;
        tx.flush().await?;
        let orig_move_id = created_mv
            .ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::invariant("Created procurement move returned no id."))?;

        tx.execute(
            "INSERT INTO stock_move_move_rel (move_orig_id, move_dest_id) \
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&orig_move_id, &dest_move_id],
        )
        .await?;
        created_upstream.push(orig_move_id);
    }
    Ok(created_upstream)
}

/// Synchronize multi-step reception and delivery rules on `warehouse_ids` after
/// `reception_steps` or `delivery_steps` changes.
pub(crate) async fn sync_warehouse_step_rules(
    env: &Env,
    tx: &mut Transaction,
    warehouse_ids: &[i64],
) -> Result<()> {
    if warehouse_ids.is_empty() {
        return Ok(());
    }
    let sudo = env.sudo(true);
    let route_model = sudo.records("stock.route")?;
    let rule_model = sudo.records("stock.rule")?;

    let rows = tx
        .query(
            "SELECT id::int8, name, company_id::int8, reception_steps, delivery_steps, \
                    lot_stock_id::int8, wh_input_stock_loc_id::int8, wh_qc_stock_loc_id::int8, \
                    wh_pack_stock_loc_id::int8, wh_output_stock_loc_id::int8, \
                    in_type_id::int8, int_type_id::int8, pick_type_id::int8, \
                    pack_type_id::int8, out_type_id::int8, store_type_id::int8, qc_type_id::int8 \
             FROM stock_warehouse WHERE id = ANY($1) ORDER BY id",
            &[&warehouse_ids],
        )
        .await?;

    let customer_loc: Option<i64> = tx
        .query(
            "SELECT id::int8 FROM stock_location WHERE usage = 'customer' AND active IS TRUE ORDER BY id LIMIT 1",
            &[],
        )
        .await?
        .first()
        .map(|r| r.try_get(0))
        .transpose()
        .map_err(db_err)?;

    for row in rows {
        let wh_id: i64 = row.try_get(0).map_err(db_err)?;
        let wh_name: String = row.try_get(1).map_err(db_err)?;
        let company_id: i64 = row.try_get(2).map_err(db_err)?;
        let reception: String = row.try_get(3).map_err(db_err)?;
        let delivery: String = row.try_get(4).map_err(db_err)?;
        let stock_loc: i64 = row.try_get(5).map_err(db_err)?;
        let input_loc: Option<i64> = row.try_get(6).map_err(db_err)?;
        let qc_loc: Option<i64> = row.try_get(7).map_err(db_err)?;
        let pack_loc: Option<i64> = row.try_get(8).map_err(db_err)?;
        let output_loc: Option<i64> = row.try_get(9).map_err(db_err)?;
        let _in_type: Option<i64> = row.try_get(10).map_err(db_err)?;
        let int_type: Option<i64> = row.try_get(11).map_err(db_err)?;
        let pick_type: Option<i64> = row.try_get(12).map_err(db_err)?;
        let pack_type: Option<i64> = row.try_get(13).map_err(db_err)?;
        let out_type: Option<i64> = row.try_get(14).map_err(db_err)?;
        let store_type: Option<i64> = row.try_get(15).map_err(db_err)?;
        let qc_type: Option<i64> = row.try_get(16).map_err(db_err)?;

        // Deactivate stale multi-step reception rules before activating the chosen step count.
        tx.execute(
            "UPDATE stock_rule SET active = false \
             WHERE warehouse_id = $1 AND company_id = $2 AND action = 'push' \
               AND route_id IN (SELECT id FROM stock_route WHERE name IN ($3, $4) AND company_id = $2)",
            &[
                &wh_id,
                &company_id,
                &format!("{wh_name}: Receive in 2 steps"),
                &format!("{wh_name}: Receive in 3 steps"),
            ],
        )
        .await?;
        // Multi-step reception push rules
        if reception == "two_steps"
            && let (Some(inp), Some(st_type)) = (input_loc, store_type.or(int_type))
        {
            tx.execute(
                "UPDATE stock_location SET active = true WHERE id = $1",
                &[&inp],
            )
            .await?;
            let route_id = ensure_route(
                &route_model,
                tx,
                company_id,
                &format!("{wh_name}: Receive in 2 steps"),
            )
            .await?;
            ensure_rule(
                &rule_model,
                tx,
                route_id,
                wh_id,
                company_id,
                &format!("{wh_name}: Inbound → Stock"),
                "push",
                "make_to_stock",
                inp,
                stock_loc,
                st_type,
                10,
            )
            .await?;
        } else if reception == "three_steps"
            && let (Some(inp), Some(qc), Some(q_type), Some(st_type)) = (
                input_loc,
                qc_loc,
                qc_type.or(int_type),
                store_type.or(int_type),
            )
        {
            tx.execute(
                "UPDATE stock_location SET active = true WHERE id = ANY($1)",
                &[&vec![inp, qc]],
            )
            .await?;
            let route_id = ensure_route(
                &route_model,
                tx,
                company_id,
                &format!("{wh_name}: Receive in 3 steps"),
            )
            .await?;
            ensure_rule(
                &rule_model,
                tx,
                route_id,
                wh_id,
                company_id,
                &format!("{wh_name}: Inbound → Quality Control"),
                "push",
                "make_to_stock",
                inp,
                qc,
                q_type,
                10,
            )
            .await?;
            ensure_rule(
                &rule_model,
                tx,
                route_id,
                wh_id,
                company_id,
                &format!("{wh_name}: Quality Control → Stock"),
                "push",
                "make_to_stock",
                qc,
                stock_loc,
                st_type,
                20,
            )
            .await?;
        }

        // Deactivate stale multi-step delivery rules before activating the chosen step count.
        tx.execute(
            "UPDATE stock_rule SET active = false \
             WHERE warehouse_id = $1 AND company_id = $2 AND action = 'pull' \
               AND route_id IN (SELECT id FROM stock_route WHERE name IN ($3, $4) AND company_id = $2)",
            &[
                &wh_id,
                &company_id,
                &format!("{wh_name}: Deliver in 2 steps"),
                &format!("{wh_name}: Deliver in 3 steps"),
            ],
        )
        .await?;
        // Multi-step delivery pull rules
        if let Some(cust) = customer_loc {
            if delivery == "pick_ship"
                && let (Some(out_loc), Some(p_type), Some(o_type)) =
                    (output_loc, pick_type.or(int_type), out_type)
            {
                tx.execute(
                    "UPDATE stock_location SET active = true WHERE id = $1",
                    &[&out_loc],
                )
                .await?;
                let route_id = ensure_route(
                    &route_model,
                    tx,
                    company_id,
                    &format!("{wh_name}: Deliver in 2 steps"),
                )
                .await?;
                ensure_rule(
                    &rule_model,
                    tx,
                    route_id,
                    wh_id,
                    company_id,
                    &format!("{wh_name}: Stock → Output (Pick)"),
                    "pull",
                    "make_to_stock",
                    stock_loc,
                    out_loc,
                    p_type,
                    10,
                )
                .await?;
                ensure_rule(
                    &rule_model,
                    tx,
                    route_id,
                    wh_id,
                    company_id,
                    &format!("{wh_name}: Output → Customers (Ship)"),
                    "pull",
                    "make_to_order",
                    out_loc,
                    cust,
                    o_type,
                    20,
                )
                .await?;
            } else if delivery == "pick_pack_ship"
                && let (Some(pk_loc), Some(out_loc), Some(p_type), Some(pk_type), Some(o_type)) = (
                    pack_loc,
                    output_loc,
                    pick_type.or(int_type),
                    pack_type.or(int_type),
                    out_type,
                )
            {
                let route_id = ensure_route(
                    &route_model,
                    tx,
                    company_id,
                    &format!("{wh_name}: Deliver in 3 steps"),
                )
                .await?;
                ensure_rule(
                    &rule_model,
                    tx,
                    route_id,
                    wh_id,
                    company_id,
                    &format!("{wh_name}: Stock → Packing (Pick)"),
                    "pull",
                    "make_to_stock",
                    stock_loc,
                    pk_loc,
                    p_type,
                    10,
                )
                .await?;
                ensure_rule(
                    &rule_model,
                    tx,
                    route_id,
                    wh_id,
                    company_id,
                    &format!("{wh_name}: Packing → Output (Pack)"),
                    "pull",
                    "make_to_order",
                    pk_loc,
                    out_loc,
                    pk_type,
                    20,
                )
                .await?;
                ensure_rule(
                    &rule_model,
                    tx,
                    route_id,
                    wh_id,
                    company_id,
                    &format!("{wh_name}: Output → Customers (Ship)"),
                    "pull",
                    "make_to_order",
                    out_loc,
                    cust,
                    o_type,
                    30,
                )
                .await?;
            }
        }
    }
    tx.flush().await?;
    Ok(())
}

async fn ensure_route(
    route_model: &Records,
    tx: &mut Transaction,
    company_id: i64,
    name: &str,
) -> Result<i64> {
    let existing = tx
        .query(
            "SELECT id::int8 FROM stock_route \
             WHERE name = $1 AND company_id = $2 ORDER BY id LIMIT 1",
            &[&name, &company_id],
        )
        .await?;
    if let Some(r) = existing.first() {
        let rid: i64 = r.try_get(0).map_err(db_err)?;
        route_model
            .browse([rid])?
            .write(tx, RawVals::new().set_cell("active", Cell::Bool(true)))
            .await?;
        return Ok(rid);
    }
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str(name.into()));
    vals.insert("company_id".into(), Cell::Id(company_id));
    vals.insert("warehouse_selectable".into(), Cell::Bool(true));
    vals.insert("product_selectable".into(), Cell::Bool(true));
    let created = route_model.create(tx, vec![vals]).await?;
    tx.flush().await?;
    created
        .ids()
        .into_iter()
        .next()
        .ok_or_else(|| Error::invariant("Created warehouse route returned no id."))
}

#[allow(clippy::too_many_arguments)]
async fn ensure_rule(
    rule_model: &Records,
    tx: &mut Transaction,
    route_id: i64,
    warehouse_id: i64,
    company_id: i64,
    name: &str,
    action: &str,
    procure_method: &str,
    src_loc: i64,
    dest_loc: i64,
    picking_type_id: i64,
    sequence: i32,
) -> Result<()> {
    let exists: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM stock_rule \
             WHERE route_id = $1 AND location_src_id = $2 AND location_dest_id = $3 \
               AND action = $4 AND active IS TRUE",
            &[&route_id, &src_loc, &dest_loc, &action],
        )
        .await?[0]
        .try_get(0)
        .map_err(db_err)?;
    if exists > 0 {
        return Ok(());
    }
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str(name.into()));
    vals.insert("action".into(), Cell::Str(action.into()));
    vals.insert("procure_method".into(), Cell::Str(procure_method.into()));
    vals.insert("location_src_id".into(), Cell::Id(src_loc));
    vals.insert("location_dest_id".into(), Cell::Id(dest_loc));
    vals.insert("picking_type_id".into(), Cell::Id(picking_type_id));
    vals.insert("route_id".into(), Cell::Id(route_id));
    vals.insert("warehouse_id".into(), Cell::Id(warehouse_id));
    vals.insert("company_id".into(), Cell::Id(company_id));
    vals.insert("sequence".into(), Cell::Int(sequence));
    rule_model.create(tx, vec![vals]).await?;
    Ok(())
}

/// After `done_moves` complete:
/// 1. Unlock waiting downstream moves (`move_dest_ids`) whose origin moves are all done/cancelled.
/// 2. Apply any active automatic `push` rule triggered by `location_dest_id` when no downstream move exists yet.
pub(crate) async fn propagate_push_and_downstream(
    records: &Records,
    tx: &mut Transaction,
    done_moves: &[i64],
) -> Result<()> {
    if done_moves.is_empty() {
        return Ok(());
    }
    // 1. Re-evaluate downstream waiting moves
    let downstream: Vec<i64> = tx
        .query(
            "SELECT DISTINCT r.move_dest_id::int8 \
             FROM stock_move_move_rel r \
             JOIN stock_move d ON d.id = r.move_dest_id \
             WHERE r.move_orig_id = ANY($1) AND d.state = 'waiting' \
               AND NOT EXISTS ( \
                 SELECT 1 FROM stock_move_move_rel r2 \
                 JOIN stock_move o ON o.id = r2.move_orig_id \
                 WHERE r2.move_dest_id = d.id AND o.state NOT IN ('done', 'cancel') \
               ) \
             ORDER BY 1",
            &[&done_moves],
        )
        .await?
        .into_iter()
        .map(|r| r.try_get(0))
        .collect::<std::result::Result<Vec<i64>, _>>()
        .map_err(db_err)?;

    if !downstream.is_empty() {
        // Lock move roots BEFORE mutating stock_move.state to respect lock hierarchy.
        // Do not invoke assign_moves inside the same validation transaction so we never violate
        // quant_engine::HeldKeys ordering; unlocked downstream moves become ready for assignment.
        let roots = crate::move_counters::lock_move_roots(tx, &downstream, &[]).await?;
        tx.execute(
            "UPDATE stock_move SET state = 'confirmed' WHERE id = ANY($1) AND state = 'waiting'",
            &[&downstream],
        )
        .await?;
        records
            .env()
            .records("stock.move")?
            .browse(downstream)?
            .modified_external(tx, &["state"])
            .await?;
        crate::move_counters::notify_pickings(records, tx, &roots).await?;
    }

    // 2. Push rules from completed moves' destination location (if no downstream move exists)
    let push_candidates = tx
        .query(
            "SELECT m.id::int8, m.company_id::int8, m.product_id::int8, m.quantity, m.uom_id::int8, \
                    m.location_dest_id::int8, m.origin, m.partner_id::int8, \
                    sr.id::int8, sr.location_dest_id::int8, sr.picking_type_id::int8 \
             FROM stock_move m \
             JOIN stock_rule sr ON sr.location_src_id = m.location_dest_id \
               AND sr.active IS TRUE AND sr.action = 'push' \
               AND (sr.company_id = m.company_id OR sr.company_id IS NULL) \
             JOIN stock_route rt ON rt.id = sr.route_id AND rt.active IS TRUE \
             WHERE m.id = ANY($1) AND m.quantity > 0 \
               AND NOT EXISTS (SELECT 1 FROM stock_move_move_rel r WHERE r.move_orig_id = m.id) \
             ORDER BY m.id, rt.sequence, sr.sequence, sr.id",
            &[&done_moves],
        )
        .await?;
    if push_candidates.is_empty() {
        return Ok(());
    }
    let env = records.env();
    let picking_model = env.records("stock.picking")?;
    let move_model = env.records("stock.move")?;
    let mut handled = BTreeSet::new();
    let mut pushed_moves = Vec::new();

    for row in push_candidates {
        let orig_move_id: i64 = row.try_get(0).map_err(db_err)?;
        if !handled.insert(orig_move_id) {
            continue;
        }
        let company_id: i64 = row.try_get(1).map_err(db_err)?;
        let product_id: i64 = row.try_get(2).map_err(db_err)?;
        let qty: Decimal = row.try_get(3).map_err(db_err)?;
        let uom_id: i64 = row.try_get(4).map_err(db_err)?;
        let src_loc: i64 = row.try_get(5).map_err(db_err)?;
        let origin: Option<String> = row.try_get(6).map_err(db_err)?;
        let partner_id: Option<i64> = row.try_get(7).map_err(db_err)?;
        let rule_id: i64 = row.try_get(8).map_err(db_err)?;
        let dst_loc: i64 = row.try_get(9).map_err(db_err)?;
        let picking_type_id: i64 = row.try_get(10).map_err(db_err)?;

        let mut pick_vals = RawVals::new();
        pick_vals.insert("company_id".into(), Cell::Id(company_id));
        pick_vals.insert("picking_type_id".into(), Cell::Id(picking_type_id));
        pick_vals.insert("location_id".into(), Cell::Id(src_loc));
        pick_vals.insert("location_dest_id".into(), Cell::Id(dst_loc));
        if let Some(ref orig) = origin {
            pick_vals.insert("origin".into(), Cell::Str(orig.as_str().into()));
        }
        if let Some(pid) = partner_id {
            pick_vals.insert("partner_id".into(), Cell::Id(pid));
        }
        let created_pick = picking_model.create(tx, vec![pick_vals]).await?;
        tx.flush().await?;
        let pick_id = created_pick
            .ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::invariant("Created push transfer returned no id."))?;

        let mut mv_vals = RawVals::new();
        mv_vals.insert("company_id".into(), Cell::Id(company_id));
        mv_vals.insert("picking_id".into(), Cell::Id(pick_id));
        mv_vals.insert("picking_type_id".into(), Cell::Id(picking_type_id));
        mv_vals.insert("product_id".into(), Cell::Id(product_id));
        mv_vals.insert("product_uom_qty".into(), Cell::Decimal(qty));
        mv_vals.insert("uom_id".into(), Cell::Id(uom_id));
        mv_vals.insert("location_id".into(), Cell::Id(src_loc));
        mv_vals.insert("location_dest_id".into(), Cell::Id(dst_loc));
        mv_vals.insert("rule_id".into(), Cell::Id(rule_id));
        if let Some(orig) = origin {
            mv_vals.insert("origin".into(), Cell::Str(orig.into()));
        }
        let created_mv = move_model.create(tx, vec![mv_vals]).await?;
        tx.flush().await?;
        if let Some(dest_mv_id) = created_mv.ids().into_iter().next() {
            tx.execute(
                "INSERT INTO stock_move_move_rel (move_orig_id, move_dest_id) \
                 VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&orig_move_id, &dest_mv_id],
            )
            .await?;
            pushed_moves.push(dest_mv_id);
        }
    }
    if !pushed_moves.is_empty() {
        // Confirm pushed moves without calling assign_moves inside this validation transaction
        // so we never violate quant_engine::HeldKeys lock ordering.
        crate::confirm::confirm_moves(records, tx, &pushed_moves, false).await?;
    }
    Ok(())
}
