// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `stock_barcode` for ERPForAll: interactive barcode scanning sessions for transfers
//! (receipts, deliveries, internal transfers) and physical inventory counts, with GS1-128 AI
//! decoding and combined barcode separator splitting (`stock.barcode_separator`).

use std::collections::BTreeMap;
use std::str::FromStr;

use erp_addon_barcodes_gs1_nomenclature::{parse_gs1_elements, split_combined_barcode};
use erp_addon_base::ir_config_setting::read_values;
use erp_addon_stock::StockPicking;
use erp_sdk::sequence::draw_by_code;
use erp_sdk::views::wire::SettingsScope;
use erp_sdk::{CompanyId, Operation, ValueMap, prelude::*};

erp_sdk::addon!(
    name = "stock_barcode",
    exact_only = true,
    depends = ["base", "product", "barcodes_gs1_nomenclature", "stock"]
);

const SEP_KEY: &str = "stock.barcode_separator";

fn default_now(_ctx: &erp_sdk::DefaultContext) -> Value {
    Value::DateTime(DateTime::now())
}

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Barcode session data could not be read.").with_source(e)
}

/// Decoded scan payload extracted from a raw scanner string (GS1-128, combined separator, or plain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedScan {
    /// Product lookup key (GTIN, barcode, or SKU/default_code).
    pub product_key: String,
    /// Optional lot or serial number parsed from GS1 AI 10/21 or combined barcode token.
    pub lot_name: Option<String>,
    /// Scanned quantity (defaults to `1` when not encoded in GS1 AI 30).
    pub quantity: Decimal,
}

/// Parse a raw scanned string using GS1-128 decoding first, falling back to combined separator
/// splitting (`product_key<sep>lot_name[<sep>qty]`) or a plain barcode token.
pub fn decode_scan_input(raw: &str, separator: &str) -> Result<DecodedScan> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Error::user("Scan or enter a barcode first."));
    }
    if trimmed.contains('(') || trimmed.starts_with("]C1") || trimmed.starts_with("]d2") {
        let elements = parse_gs1_elements(trimmed)?;
        let mut product_key = None;
        let mut lot_name = None;
        let mut quantity = Decimal::ONE;
        for el in elements {
            match el.kind.as_str() {
                "product" => product_key = Some(el.value),
                "lot" | "serial" => lot_name = Some(el.value),
                "quantity" => {
                    let parsed = Decimal::from_str(&el.value).map_err(|_| {
                        Error::validation(format!("Invalid GS1 quantity '{}'.", el.value))
                    })?;
                    if parsed <= Decimal::ZERO {
                        return Err(Error::validation("Scanned quantity must be positive."));
                    }
                    quantity = parsed;
                }
                _ => {}
            }
        }
        if let Some(pk) = product_key {
            return Ok(DecodedScan {
                product_key: pk,
                lot_name,
                quantity,
            });
        }
    }

    let parts = split_combined_barcode(trimmed, separator)?;
    let product_key = parts
        .first()
        .cloned()
        .ok_or_else(|| Error::user("Scan or enter a barcode first."))?;
    let lot_name = parts.get(1).cloned().filter(|s| !s.is_empty());
    let quantity = if let Some(q_str) = parts.get(2) {
        let parsed = Decimal::from_str(q_str).map_err(|_| {
            Error::validation(format!("Invalid quantity '{q_str}' in combined barcode."))
        })?;
        if parsed <= Decimal::ZERO {
            return Err(Error::validation("Scanned quantity must be positive."));
        }
        parsed
    } else {
        Decimal::ONE
    };
    Ok(DecodedScan {
        product_key,
        lot_name,
        quantity,
    })
}

async fn global_barcode_separator(tx: &mut Transaction) -> Result<String> {
    let env = tx.env().sudo(true);
    let read = read_values(&env, tx, SettingsScope::Global {}, vec![SEP_KEY.to_owned()]).await?;
    let json = erp_sdk::serde_json::to_value(&read)
        .map_err(|e| Error::internal(format!("barcode separator read: {e}")))?;
    let entry = json["values"]
        .as_array()
        .and_then(|arr| arr.iter().find(|v| v["key"] == SEP_KEY))
        .ok_or_else(|| {
            Error::internal(format!(
                "Global setting '{SEP_KEY}' is missing from response."
            ))
        })?;
    let sep = entry["value"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::validation(format!(
                "Global setting '{SEP_KEY}' must be a non-empty string."
            ))
        })?;
    Ok(sep.to_owned())
}

async fn next_session_name(env: &Env, tx: &mut Transaction, company_id: i64) -> Result<String> {
    let company = CompanyId::new(company_id)?;
    let today = env.context_today()?;
    draw_by_code(
        tx,
        &env.sudo(true),
        "stock.barcode.session",
        company,
        Some(today),
    )
    .await
}

/// `stock.barcode.session`: interactive barcode scanning session for a transfer or physical count.
#[model(
    name = "stock.barcode.session",
    description = "Barcode Scanning Session",
    order = "id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct StockBarcodeSession {
    /// Session reference (e.g. `SCAN/00001`).
    #[field(
        char,
        required = true,
        default = "New",
        copy = false,
        string = "Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Transfer picking or physical inventory counting mode.
    #[field(
        selection,
        selection = [
            ("transfer", "Transfer (Receipt / Delivery / Internal)"),
            ("inventory", "Inventory Count")
        ],
        default = "transfer",
        required = true,
        string = "Mode"
    )]
    pub mode: Option<Arc<str>>,
    /// Target transfer when in `transfer` mode.
    #[field(
        many2one,
        comodel = "stock.picking",
        check_company = true,
        string = "Transfer"
    )]
    pub picking_id: Option<RecId>,
    /// Target internal location when in `inventory` mode.
    #[field(
        many2one,
        comodel = "stock.location",
        check_company = true,
        string = "Location"
    )]
    pub location_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Current scanned barcode input.
    #[field(char, string = "Scan Barcode")]
    pub barcode_input: Option<Arc<str>>,
    /// Feedback message from the most recent scan.
    #[field(char, readonly = true, string = "Last Scan Result")]
    pub last_message: Option<Arc<str>>,
    /// Total number of scans recorded in this session.
    #[field(integer, compute = "_compute_scanned_count", string = "Lines Scanned")]
    pub scanned_count: i32,
    /// Lifecycle status.
    #[field(
        selection,
        selection = [
            ("open", "In Progress"),
            ("done", "Completed"),
            ("cancel", "Cancelled")
        ],
        default = "open",
        required = true,
        readonly = true,
        copy = false,
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Scanned barcode log lines.
    #[field(
        one2many,
        comodel = "stock.barcode.scan.line",
        inverse_name = "session_id",
        string = "Scanned Items"
    )]
    pub line_ids: Vec<RecId>,
}

/// `stock.barcode.scan.line`: one scanned barcode entry inside a [`StockBarcodeSession`].
#[model(
    name = "stock.barcode.scan.line",
    description = "Barcode Scan Line",
    order = "id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct StockBarcodeScanLine {
    /// Parent scan session.
    #[field(
        many2one,
        comodel = "stock.barcode.session",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Session"
    )]
    pub session_id: Option<RecId>,
    /// Company inherited from session.
    #[field(
        many2one,
        comodel = "res.company",
        related = "session_id.company_id",
        store = true,
        required = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Raw barcode string scanned.
    #[field(char, required = true, string = "Barcode")]
    pub barcode: Option<Arc<str>>,
    /// Matched product variant.
    #[field(
        many2one,
        comodel = "product.product",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Matched or decoded lot/serial name.
    #[field(char, string = "Lot/Serial")]
    pub lot_name: Option<Arc<str>>,
    /// Location associated with this scan.
    #[field(
        many2one,
        comodel = "stock.location",
        check_company = true,
        string = "Location"
    )]
    pub location_id: Option<RecId>,
    /// Quantity scanned.
    #[field(float, digits = "Product Unit", default = 1, string = "Quantity")]
    pub quantity: Decimal,
    /// UTC timestamp of the scan.
    #[field(datetime, required = true, default_fn = default_now, string = "Scanned On")]
    pub scanned_at: Option<DateTime>,
    /// Resolution summary.
    #[field(char, string = "Note")]
    pub note: Option<Arc<str>>,
}

#[methods(StockBarcodeSession, addon = "stock_barcode")]
impl StockBarcodeSession {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<StockBarcodeSession>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut out: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        let active_company = this.env().company_id()?;
        if fields.iter().any(|f| f == "company_id")
            && !out.contains_key("company_id")
            && let Some(cid) = active_company
        {
            out.insert("company_id".into(), Value::Int(cid));
        }
        if fields.iter().any(|f| f == "location_id")
            && !out.contains_key("location_id")
            && let Some(cid) = active_company
        {
            let rows = tx
                .query(
                    "SELECT lot_stock_id::int8 FROM stock_warehouse \
                     WHERE company_id = $1 AND active IS TRUE ORDER BY id LIMIT 1",
                    &[&cid],
                )
                .await?;
            if let Some(row) = rows.first()
                && let Some(loc_id) = row.try_get::<_, Option<i64>>(0).map_err(db_err)?
            {
                out.insert("location_id".into(), Value::Int(loc_id));
            }
        }
        Ok(out)
    }

    #[compute(depends = ["line_ids"])]
    async fn _compute_scanned_count(
        this: &RecordSet<StockBarcodeSession>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockBarcodeSession>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|id| id.as_real().map(|r| r.get()))
            .collect();
        let mut counts: BTreeMap<i64, i32> = BTreeMap::new();
        if !real_ids.is_empty() {
            let rows = tx
                .query(
                    "SELECT session_id::int8, count(*)::int4 \
                     FROM stock_barcode_scan_line WHERE session_id = ANY($1) GROUP BY session_id",
                    &[&real_ids],
                )
                .await?;
            for row in rows {
                counts.insert(
                    row.try_get(0).map_err(db_err)?,
                    row.try_get(1).map_err(db_err)?,
                );
            }
        }
        for (idx, rec_id) in rec_ids.iter().enumerate() {
            let cnt = rec_id
                .as_real()
                .and_then(|id| counts.get(&id.get()).copied())
                .unwrap_or(0);
            out.set(idx, StockBarcodeSession::SCANNED_COUNT, cnt)?;
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<StockBarcodeSession>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        let env = this.env();
        let active_company = env.company_id()?;
        for vals in &mut vals_list {
            if vals.contains("state") {
                return Err(Error::validation(
                    "Barcode session status changes only through its session actions.",
                ));
            }
            if let Some(Cell::Str(m)) = vals.get("mode")
                && !matches!(m.as_ref(), "transfer" | "inventory")
            {
                return Err(Error::validation(format!(
                    "Unsupported barcode session mode '{m}'."
                )));
            }
            let company_id = match vals.get("company_id") {
                Some(Cell::Id(cid)) => *cid,
                Some(Cell::Int(cid)) => i64::from(*cid),
                None | Some(Cell::Null) => {
                    let cid = active_company.ok_or_else(|| {
                        Error::validation("A company is required to create a barcode session.")
                    })?;
                    vals.insert("company_id".into(), Cell::Id(cid));
                    cid
                }
                other => {
                    return Err(Error::validation(format!(
                        "Invalid company_id for barcode session: {other:?}"
                    )));
                }
            };
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let name = next_session_name(env, tx, company_id).await?;
                vals.insert("name".into(), Cell::Str(name.into()));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<StockBarcodeSession>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("state") {
            return Err(Error::validation(
                "Barcode session status changes only through its session actions.",
            ));
        }
        if let Some(Cell::Str(m)) = vals.get("mode")
            && !matches!(m.as_ref(), "transfer" | "inventory")
        {
            return Err(Error::validation(format!(
                "Unsupported barcode session mode '{m}'."
            )));
        }
        let ids = this.ids();
        if !ids.is_empty() {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT count(*)::int8 FROM stock_barcode_session \
                     WHERE id = ANY($1) AND state <> 'open'",
                    &[&ids],
                )
                .await?;
            let closed: i64 = rows
                .first()
                .ok_or_else(|| Error::internal("Barcode session state check returned no row."))?
                .try_get(0)
                .map_err(db_err)?;
            if closed > 0 {
                return Err(Error::validation(
                    "Completed or cancelled barcode sessions cannot be modified.",
                ));
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<StockBarcodeSession>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        let ids = this.ids();
        if !ids.is_empty() {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT count(*)::int8 FROM stock_barcode_session \
                     WHERE id = ANY($1) AND state = 'done'",
                    &[&ids],
                )
                .await?;
            let done: i64 = rows
                .first()
                .ok_or_else(|| Error::internal("Barcode session unlink check returned no row."))?
                .try_get(0)
                .map_err(db_err)?;
            if done > 0 {
                return Err(Error::user("Completed barcode sessions cannot be deleted."));
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Process the current `barcode_input` string (GS1-128, combined separator, product SKU/barcode,
    /// or location barcode) and update the active transfer or count session.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_process_barcode(
        this: &RecordSet<StockBarcodeSession>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let session_id = records.ensure_one()?.int()?;
        let env = records.env().clone();
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT state, mode, picking_id::int8, location_id::int8, company_id::int8, barcode_input \
                     FROM stock_barcode_session WHERE id = $1 FOR UPDATE",
                    &[&session_id],
                )
                .await?;
            let Some(row) = rows.first() else {
                return Err(Error::missing("Barcode session not found."));
            };
            let state: String = row.try_get(0).map_err(db_err)?;
            if state != "open" {
                return Err(Error::user("Only open barcode sessions can accept scans."));
            }
            let mode: String = row.try_get(1).map_err(db_err)?;
            let picking_id: Option<i64> = row.try_get(2).map_err(db_err)?;
            let location_id: Option<i64> = row.try_get(3).map_err(db_err)?;
            let company_id: i64 = row.try_get(4).map_err(db_err)?;
            let raw_input: Option<String> = row.try_get(5).map_err(db_err)?;
            let raw = raw_input.unwrap_or_default();
            let sep = global_barcode_separator(tx).await?;
            let decoded = decode_scan_input(&raw, &sep)?;

            let uid = env.uid();
            // First check if a location barcode was scanned (in inventory mode).
            if mode == "inventory" && decoded.lot_name.is_none() {
                let loc_rows = tx
                    .query(
                        "SELECT id::int8, complete_name FROM stock_location \
                         WHERE barcode = $1 AND active IS TRUE \
                           AND (company_id IS NULL OR company_id = $2) LIMIT 1",
                        &[&decoded.product_key, &company_id],
                    )
                    .await?;
                if let Some(lrow) = loc_rows.first() {
                    let loc_id: i64 = lrow.try_get(0).map_err(db_err)?;
                    let loc_name: String = lrow.try_get(1).map_err(db_err)?;
                    let msg = format!("Active location switched to {loc_name}.");
                    tx.execute(
                        "UPDATE stock_barcode_session \
                         SET location_id = $2, barcode_input = NULL, last_message = $3, \
                             write_uid = $4, write_date = (now() AT TIME ZONE 'UTC') \
                         WHERE id = $1",
                        &[&session_id, &loc_id, &msg, &uid],
                    )
                    .await?;
                    records
                        .modified_external(tx, &["location_id", "barcode_input", "last_message"])
                        .await?;
                    return Ok(());
                }
            }

            // Look up product by barcode or default_code scoped to the session's company.
            let prod_rows = tx
                .query(
                    "SELECT pp.id::int8, pt.uom_id::int8 \
                     FROM product_product pp \
                     JOIN product_template pt ON pt.id = pp.product_tmpl_id \
                     WHERE pp.active IS TRUE \
                       AND (pt.company_id IS NULL OR pt.company_id = $2) \
                       AND ( \
                         lower(COALESCE(pp.barcode, '')) = lower($1) \
                         OR lower(COALESCE(pp.default_code, '')) = lower($1) \
                         OR lower(COALESCE(pt.barcode, '')) = lower($1) \
                         OR lower(COALESCE(pt.default_code, '')) = lower($1) \
                       ) ORDER BY pt.company_id NULLS LAST, pp.id LIMIT 1",
                    &[&decoded.product_key, &company_id],
                )
                .await?;
            let Some(prow) = prod_rows.first() else {
                return Err(Error::user(format!(
                    "No product or location matches barcode '{}'.",
                    decoded.product_key
                )));
            };
            let product_id: i64 = prow.try_get(0).map_err(db_err)?;
            let uom_id: i64 = prow.try_get(1).map_err(db_err)?;
            let prod_rec = env.records("product.product")?.browse(vec![product_id])?;
            prod_rec.check_access(tx, Operation::Read).await?;
            let product_label = match prod_rec.cell(tx, "display_name").await? {
                Cell::Str(s) if !s.trim().is_empty() => s.to_string(),
                _ => decoded.product_key.clone(),
            };

            let mut line_location_id = location_id;
            let note = match mode.as_str() {
                "transfer" => {
                    let Some(pid) = picking_id else {
                        return Err(Error::user(
                            "Select a transfer before scanning items in Transfer mode.",
                        ));
                    };
                    let p_rows = tx
                        .query(
                            "SELECT state, location_id::int8, location_dest_id::int8 \
                             FROM stock_picking WHERE id = $1 FOR UPDATE",
                            &[&pid],
                        )
                        .await?;
                    let Some(pk) = p_rows.first() else {
                        return Err(Error::missing("Transfer not found."));
                    };
                    let pstate: String = pk.try_get(0).map_err(db_err)?;
                    let p_src: i64 = pk.try_get(1).map_err(db_err)?;
                    let p_dst: i64 = pk.try_get(2).map_err(db_err)?;
                    line_location_id = Some(p_dst);
                    if matches!(pstate.as_str(), "done" | "cancel") {
                        return Err(Error::user(
                            "Cannot scan items onto a completed or cancelled transfer.",
                        ));
                    }
                    if pstate == "draft" {
                        env.records("stock.picking")?
                            .browse(vec![pid])?
                            .call::<(), ()>(tx, "action_confirm", ())
                            .await?;
                        tx.flush().await?;
                    }
                    // Find an open move line for this product (matching lot_name if provided) or its move.
                    let ml_rows = tx
                        .query(
                            "SELECT id::int8, quantity FROM stock_move_line \
                             WHERE picking_id = $1 AND product_id = $2 \
                               AND membership = 'open' AND active IS TRUE \
                               AND ($3::text IS NULL OR COALESCE(lot_name, '') = $3) \
                             ORDER BY id LIMIT 1",
                            &[&pid, &product_id, &decoded.lot_name],
                        )
                        .await?;
                    if let Some(ml) = ml_rows.first() {
                        let ml_id: i64 = ml.try_get(0).map_err(db_err)?;
                        let cur_qty: Decimal = ml.try_get(1).map_err(db_err)?;
                        let next_qty = cur_qty + decoded.quantity;
                        let mut wvals =
                            RawVals::new().set_cell("quantity", Cell::Decimal(next_qty));
                        if let Some(lot) = &decoded.lot_name {
                            wvals.insert("lot_name".into(), Cell::Str(lot.as_str().into()));
                        }
                        env.records("stock.move.line")?
                            .browse(vec![ml_id])?
                            .write(tx, wvals)
                            .await?;
                    } else {
                        // Find or create a move on this picking, then create an open move line.
                        let mv_rows = tx
                            .query(
                                "SELECT id::int8, uom_id::int8 FROM stock_move \
                                 WHERE picking_id = $1 AND product_id = $2 \
                                   AND state NOT IN ('done', 'cancel') ORDER BY id LIMIT 1",
                                &[&pid, &product_id],
                            )
                            .await?;
                        let (move_id, mv_uom_id) = if let Some(mv) = mv_rows.first() {
                            (
                                mv.try_get::<_, i64>(0).map_err(db_err)?,
                                mv.try_get::<_, i64>(1).map_err(db_err)?,
                            )
                        } else {
                            let mut mvals = RawVals::new();
                            mvals.insert("picking_id".into(), Cell::Id(pid));
                            mvals.insert("product_id".into(), Cell::Id(product_id));
                            mvals.insert("product_uom_qty".into(), Cell::Decimal(decoded.quantity));
                            mvals.insert("uom_id".into(), Cell::Id(uom_id));
                            mvals.insert("location_id".into(), Cell::Id(p_src));
                            mvals.insert("location_dest_id".into(), Cell::Id(p_dst));
                            let created_mv = env
                                .records("stock.move")?
                                .create(tx, vec![mvals])
                                .await?;
                            let mid = created_mv
                                .ids()
                                .into_iter()
                                .next()
                                .ok_or_else(|| {
                                    Error::invariant("Created stock.move returned no id.")
                                })?;
                            (mid, uom_id)
                        };
                        let mut lvals = RawVals::new();
                        lvals.insert("picking_id".into(), Cell::Id(pid));
                        lvals.insert("move_id".into(), Cell::Id(move_id));
                        lvals.insert("product_id".into(), Cell::Id(product_id));
                        lvals.insert("uom_id".into(), Cell::Id(mv_uom_id));
                        lvals.insert("location_id".into(), Cell::Id(p_src));
                        lvals.insert("location_dest_id".into(), Cell::Id(p_dst));
                        lvals.insert("quantity".into(), Cell::Decimal(decoded.quantity));
                        if let Some(lot) = &decoded.lot_name {
                            lvals.insert("lot_name".into(), Cell::Str(lot.as_str().into()));
                        }
                        env.records("stock.move.line")?
                            .create(tx, vec![lvals])
                            .await?;
                    }
                    format!(
                        "Added +{} of {} to transfer.",
                        decoded.quantity, product_label
                    )
                }
                "inventory" => {
                    if location_id.is_none() {
                        return Err(Error::user(
                            "Select or scan a Location before counting items in Inventory mode.",
                        ));
                    }
                    format!("Counted +{} of {}.", decoded.quantity, product_label)
                }
                other => {
                    return Err(Error::validation(format!(
                        "Unsupported barcode session mode '{other}'."
                    )));
                }
            };

            let mut scan_vals = RawVals::new();
            scan_vals.insert("session_id".into(), Cell::Id(session_id));
            scan_vals.insert("barcode".into(), Cell::Str(raw.trim().into()));
            scan_vals.insert("product_id".into(), Cell::Id(product_id));
            if let Some(lot) = &decoded.lot_name {
                scan_vals.insert("lot_name".into(), Cell::Str(lot.as_str().into()));
            }
            if let Some(loc) = line_location_id {
                scan_vals.insert("location_id".into(), Cell::Id(loc));
            }
            scan_vals.insert("quantity".into(), Cell::Decimal(decoded.quantity));
            scan_vals.insert("note".into(), Cell::Str(note.as_str().into()));
            env.records("stock.barcode.scan.line")?
                .create(tx, vec![scan_vals])
                .await?;

            tx.execute(
                "UPDATE stock_barcode_session SET barcode_input = NULL, last_message = $2 WHERE id = $1",
                &[&session_id, &note],
            )
            .await?;
            records
                .modified_external(tx, &["barcode_input", "last_message"])
                .await?;
            Ok(())
        })
        .await
    }

    /// Complete the scanning session: validates the linked transfer or applies scanned inventory counts.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_validate_session(
        this: &RecordSet<StockBarcodeSession>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let session_id = records.ensure_one()?.int()?;
        let env = records.env().clone();
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT state, mode, picking_id::int8, location_id::int8, company_id::int8 \
                     FROM stock_barcode_session WHERE id = $1 FOR UPDATE",
                    &[&session_id],
                )
                .await?;
            let Some(row) = rows.first() else {
                return Err(Error::missing("Barcode session not found."));
            };
            let state: String = row.try_get(0).map_err(db_err)?;
            if state != "open" {
                return Err(Error::user("Only open barcode sessions can be validated."));
            }
            let mode: String = row.try_get(1).map_err(db_err)?;
            let picking_id: Option<i64> = row.try_get(2).map_err(db_err)?;
            let default_loc: Option<i64> = row.try_get(3).map_err(db_err)?;
            let company_id: i64 = row.try_get(4).map_err(db_err)?;

            let uid = env.uid();
            let msg = match mode.as_str() {
                "transfer" => {
                    let Some(pid) = picking_id else {
                        return Err(Error::user("Select a transfer before validating."));
                    };
                    env.records("stock.picking")?
                        .browse(vec![pid])?
                        .call::<(), Value>(tx, "action_validate_transfer", ())
                        .await?;
                    "Transfer validated via barcode session.".to_owned()
                }
                "inventory" => {
                    if !env.su() && !tx.has_group(&env, "stock.group_stock_manager").await? {
                        return Err(Error::access(
                            "Only an Inventory Administrator can apply barcode inventory counts.",
                        ));
                    }
                    let agg_rows = tx
                        .query(
                            "SELECT product_id::int8, COALESCE(location_id, $2)::int8, \
                                    NULLIF(trim(COALESCE(lot_name, '')), ''), sum(quantity) \
                             FROM stock_barcode_scan_line \
                             WHERE session_id = $1 AND product_id IS NOT NULL \
                             GROUP BY product_id, COALESCE(location_id, $2), \
                                      NULLIF(trim(COALESCE(lot_name, '')), '') \
                             ORDER BY product_id, 2, 3 NULLS LAST",
                            &[&session_id, &default_loc],
                        )
                        .await?;
                    if agg_rows.is_empty() {
                        return Err(Error::user(
                            "Scan at least one product before applying the inventory count.",
                        ));
                    }
                    let lot_model = env.records("stock.lot")?;
                    let adj_model = env.records("stock.inventory.adjustment.new")?;
                    let mut applied = 0usize;
                    for arow in agg_rows {
                        let product_id: i64 = arow.try_get(0).map_err(db_err)?;
                        let loc_id: Option<i64> = arow.try_get(1).map_err(db_err)?;
                        let Some(loc_id) = loc_id else {
                            return Err(Error::user(
                                "A location is required to apply scanned counts.",
                            ));
                        };
                        let lot_name: Option<String> = arow.try_get(2).map_err(db_err)?;
                        let counted: Decimal = arow.try_get(3).map_err(db_err)?;
                        let lot_id = if let Some(ref lname) = lot_name {
                            let existing = tx
                                .query(
                                    "SELECT id::int8 FROM stock_lot \
                                     WHERE product_id = $1 AND name = $2 \
                                       AND (company_id = $3 OR company_id IS NULL) \
                                     ORDER BY company_id NULLS LAST, id LIMIT 1",
                                    &[&product_id, &lname, &company_id],
                                )
                                .await?;
                            if let Some(found) = existing.first() {
                                Some(found.try_get::<_, i64>(0).map_err(db_err)?)
                            } else {
                                let mut lv = RawVals::new();
                                lv.insert("name".into(), Cell::Str(lname.as_str().into()));
                                lv.insert("product_id".into(), Cell::Id(product_id));
                                lv.insert("company_id".into(), Cell::Id(company_id));
                                let created = lot_model.create(tx, vec![lv]).await?;
                                tx.flush().await?;
                                Some(created.ids().into_iter().next().ok_or_else(|| {
                                    Error::invariant("Created stock.lot returned no id.")
                                })?)
                            }
                        } else {
                            None
                        };

                        // Apply count through the inventory adjustment wizard so all balances
                        // flow through the quant engine (never calling StockQuant::create).
                        let mut av = RawVals::new();
                        av.insert("company_id".into(), Cell::Id(company_id));
                        av.insert("product_id".into(), Cell::Id(product_id));
                        av.insert("location_id".into(), Cell::Id(loc_id));
                        if let Some(lid) = lot_id {
                            av.insert("lot_id".into(), Cell::Id(lid));
                        }
                        av.insert("counted_quantity".into(), Cell::Decimal(counted));
                        av.insert("note".into(), Cell::Str("Barcode Inventory Count".into()));
                        let wiz = adj_model.create(tx, vec![av]).await?;
                        let _: Value = wiz.call(tx, "action_apply", ()).await?;
                        applied += 1;
                    }
                    format!("Applied barcode inventory count to {applied} stock line(s).")
                }
                other => {
                    return Err(Error::validation(format!(
                        "Unsupported barcode session mode '{other}'."
                    )));
                }
            };

            tx.execute(
                "UPDATE stock_barcode_session \
                 SET state = 'done', last_message = $2, write_uid = $3, \
                     write_date = (now() AT TIME ZONE 'UTC') \
                 WHERE id = $1",
                &[&session_id, &msg, &uid],
            )
            .await?;
            records
                .modified_external(tx, &["state", "last_message"])
                .await?;
            Ok(())
        })
        .await
    }

    /// Cancel an open barcode scanning session.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_cancel_session(
        this: &RecordSet<StockBarcodeSession>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        let uid = records.env().uid();
        tx.flush().await?;
        let rows = tx
            .query(
                "SELECT state FROM stock_barcode_session WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let state: String = rows
            .first()
            .ok_or_else(|| Error::missing("Barcode session not found."))?
            .try_get(0)
            .map_err(db_err)?;
        if state != "open" {
            return Err(Error::user("Only open barcode sessions can be cancelled."));
        }
        tx.execute(
            "UPDATE stock_barcode_session \
             SET state = 'cancel', write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') \
             WHERE id = $1",
            &[&id, &uid],
        )
        .await?;
        records.modified_external(tx, &["state"]).await?;
        Ok(())
    }
}

#[extend(StockPicking, addon = "stock_barcode")]
pub struct StockPickingBarcode {}

#[methods(StockPicking, addon = "stock_barcode")]
impl StockPickingBarcode {
    /// Open or create a barcode scanning session bound to this transfer.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_open_barcode(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        let picking_id = records.ensure_one()?.int()?;
        let env = records.env();
        let rows = tx
            .query(
                "SELECT id::int8 FROM stock_barcode_session \
                 WHERE picking_id = $1 AND state = 'open' ORDER BY id DESC LIMIT 1",
                &[&picking_id],
            )
            .await?;
        let session_id = if let Some(row) = rows.first() {
            row.try_get::<_, i64>(0).map_err(db_err)?
        } else {
            let p_rows = tx
                .query(
                    "SELECT company_id::int8 FROM stock_picking WHERE id = $1",
                    &[&picking_id],
                )
                .await?;
            let cid: i64 = p_rows
                .first()
                .ok_or_else(|| Error::missing("Transfer not found."))?
                .try_get(0)
                .map_err(db_err)?;
            let mut vals = RawVals::new();
            vals.insert("mode".into(), Cell::Str("transfer".into()));
            vals.insert("picking_id".into(), Cell::Id(picking_id));
            vals.insert("company_id".into(), Cell::Id(cid));
            let created = env
                .records("stock.barcode.session")?
                .create(tx, vec![vals])
                .await?;
            created
                .ids()
                .into_iter()
                .next()
                .ok_or_else(|| Error::invariant("Created barcode session returned no id."))?
        };
        let mut action = ValueMap::new();
        action.insert("type".into(), Value::str("ir.actions.act_window"));
        action.insert("name".into(), Value::str("Barcode Scanning"));
        action.insert("res_model".into(), Value::str("stock.barcode.session"));
        action.insert("res_id".into(), Value::Int(session_id));
        action.insert(
            "views".into(),
            Value::list([Value::list([Value::False, Value::str("form")])]),
        );
        action.insert("target".into(), Value::str("current"));
        Ok(Value::Map(action.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_gs1_and_combined_barcodes() {
        let gs1 = decode_scan_input("(01)09501101530003(10)LOT42(30)5", ",").expect("gs1");
        assert_eq!(gs1.product_key, "09501101530003");
        assert_eq!(gs1.lot_name.as_deref(), Some("LOT42"));
        assert_eq!(gs1.quantity, Decimal::from(5));

        let combined = decode_scan_input("MS-001|LOT-9|3", "|").expect("combined");
        assert_eq!(combined.product_key, "MS-001");
        assert_eq!(combined.lot_name.as_deref(), Some("LOT-9"));
        assert_eq!(combined.quantity, Decimal::from(3));

        let plain = decode_scan_input("MS-001", ",").expect("plain");
        assert_eq!(plain.product_key, "MS-001");
        assert_eq!(plain.lot_name, None);
        assert_eq!(plain.quantity, Decimal::ONE);
    }
}
