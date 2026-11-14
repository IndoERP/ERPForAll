// SPDX-License-Identifier: MIT OR Apache-2.0
//! Configuration lifecycle of operation types. Dependent writes share a savepoint and caller rights.
use crate::warehouse::StockPickingType;
use erp_sdk::prelude::*;
use erp_sdk::{Operation, ValueMap};

async fn authorize(
    records: &Records,
    tx: &mut Transaction,
    vals: &RawVals,
    op: Operation,
) -> Result<()> {
    records.check_access(tx, op).await?;
    for name in vals
        .iter()
        .map(|(name, _)| name)
        .chain(vals.iter_commands().map(|(name, _)| name))
    {
        if records.spec().field(name).is_some()
            && !tx
                .has_field_access(records.env(), records.spec().name, name, FieldOp::Write)
                .await?
        {
            return Err(Error::access(format!(
                "Writing {}.{name} is not allowed.",
                records.spec().name
            )));
        }
    }
    Ok(())
}
fn id(vals: &RawVals, field: &str) -> Result<Option<i64>> {
    match vals.get(field) {
        None | Some(Cell::Null) => Ok(None),
        Some(Cell::Id(id)) if *id > 0 => Ok(Some(*id)),
        Some(_) => Err(Error::validation(format!(
            "stock.picking.type.{field} must be a positive record id."
        ))),
    }
}
fn string<'a>(vals: &'a RawVals, field: &str) -> Result<Option<&'a str>> {
    match vals.get(field) {
        None | Some(Cell::Null) => Ok(None),
        Some(Cell::Str(s)) => Ok(Some(s)),
        Some(_) => Err(Error::validation(format!(
            "stock.picking.type.{field} must be text."
        ))),
    }
}
#[derive(Default)]
struct LocationCache {
    external_ids: std::collections::BTreeMap<&'static str, i64>,
    company_warehouses: std::collections::BTreeMap<i64, i64>,
}
async fn prefetch_warehouses(env: &Env, tx: &mut Transaction, values: &[RawVals]) -> Result<()> {
    let mut ids = Vec::new();
    for vals in values {
        if let Some(id) = id(vals, "warehouse_id")? {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    if !ids.is_empty() {
        let warehouses = env.records("stock.warehouse")?.browse(ids)?;
        // One batch of each required column populates the ordinary, authority-checked ORM cache.
        for field in ["company_id", "lot_stock_id", "name"] {
            drop(warehouses.cells(tx, field).await?);
        }
    }
    Ok(())
}
async fn default_location(
    env: &Env,
    tx: &mut Transaction,
    name: &'static str,
    cache: &mut LocationCache,
) -> Result<i64> {
    if let Some(id) = cache.external_ids.get(name).copied() {
        env.records("stock.location")?
            .browse([id])?
            .check_access(tx, Operation::Read)
            .await?;
        return Ok(id);
    }
    // An external-id lookup is metadata, not location authority. Reading the resulting location
    // below and the base relation check both retain the caller's authorization.
    let rows=tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name=$1 AND model='stock.location'", &[&name]).await?;
    let [row] = rows.as_slice() else {
        return Err(Error::user(format!(
            "Required stock location stock.{name} is missing; install or repair Inventory default data."
        )));
    };
    let id: i64 = row
        .try_get(0)
        .map_err(|e| Error::db("Invalid stock location external id.").with_source(e))?;
    let location = env.records("stock.location")?.browse([id])?;
    location.check_access(tx, Operation::Read).await?;
    cache.external_ids.insert(name, id);
    Ok(id)
}
async fn locations(
    env: &Env,
    tx: &mut Transaction,
    vals: &mut RawVals,
    cache: &mut LocationCache,
) -> Result<()> {
    if vals.contains("default_location_src_id") && vals.contains("default_location_dest_id") {
        return Ok(());
    }
    let company = id(vals, "company_id")?.or(env.company_id()?);
    let warehouse = match id(vals, "warehouse_id")? {
        Some(id) => env.records("stock.warehouse")?.browse([id])?,
        None => {
            let company = company.ok_or_else(|| {
                Error::user(
                    "Choose a company and warehouse before configuring operation locations.",
                )
            })?;
            let found = match cache.company_warehouses.get(&company).copied() {
                Some(wid) => env.records("stock.warehouse")?.browse([wid])?,
                None => {
                    let found = env
                        .records("stock.warehouse")?
                        .search_with(
                            tx,
                            &Domain::field("company_id")?.eq(company),
                            &SearchOptions::new().limit(1),
                        )
                        .await?;
                    if found.is_empty() {
                        return Err(Error::user(
                            "No warehouse is available for the operation type company; warehouse provisioning is not supported until a later release.",
                        ));
                    }
                    cache.company_warehouses.insert(company, found.ids()[0]);
                    found
                }
            };
            vals.insert("warehouse_id".into(), Cell::Id(found.ids()[0]));
            found
        }
    };
    let owner = warehouse.cell(tx, "company_id").await?;
    if let Some(company) = company
        && owner != Cell::Id(company)
    {
        return Err(Error::validation(
            "stock.picking.type.warehouse_id must belong to its company.",
        ));
    }
    let stock = warehouse.cell(tx, "lot_stock_id").await?;
    let code = match vals.get("code") {
        None => "incoming", // declared new-record default, only for an absent value
        Some(Cell::Str(code)) => code.as_ref(),
        Some(_) => return Err(Error::validation("stock.picking.type.code must be text.")),
    };
    let incoming = code == "incoming";
    let outgoing = code == "outgoing";
    if !vals.contains("default_location_src_id") {
        let source = if incoming {
            Cell::Id(default_location(env, tx, "stock_location_suppliers", cache).await?)
        } else {
            stock.clone()
        };
        vals.insert("default_location_src_id".into(), source);
    }
    if !vals.contains("default_location_dest_id") {
        let dest = if outgoing {
            Cell::Id(default_location(env, tx, "stock_location_customers", cache).await?)
        } else {
            stock
        };
        vals.insert("default_location_dest_id".into(), dest);
    }
    Ok(())
}
async fn sequence_name(
    env: &Env,
    tx: &mut Transaction,
    vals: &RawVals,
    prefix: &str,
) -> Result<String> {
    match id(vals, "warehouse_id")? {
        Some(id) => {
            let wh = env.records("stock.warehouse")?.browse([id])?;
            let Cell::Str(name) = wh.cell(tx, "name").await? else {
                return Err(Error::internal("Warehouse has no name."));
            };
            // This is a generated record label, editable on ir.sequence afterwards.
            Ok(format!("{prefix} numbering ({name})"))
        }
        None => Ok(format!("{prefix} numbering")),
    }
}
async fn create_sequence(env: &Env, tx: &mut Transaction, vals: &mut RawVals) -> Result<()> {
    let prefix = string(vals, "sequence_code")?.map(str::to_owned);
    if let Some(prefix) = prefix
        && !prefix.is_empty()
        && id(vals, "sequence_id")?.is_none()
    {
        let company = id(vals, "company_id")?
            .or(env.company_id()?)
            .ok_or_else(|| {
                Error::user("Choose a company before creating an operation sequence.")
            })?;
        if let Some(wid) = id(vals, "warehouse_id")?
            && env
                .records("stock.warehouse")?
                .browse([wid])?
                .cell(tx, "company_id")
                .await?
                != Cell::Id(company)
        {
            return Err(Error::validation(
                "stock.picking.type.warehouse_id must belong to its company.",
            ));
        }
        let name = sequence_name(env, tx, vals, &prefix).await?;
        let sequence = env
            .records("ir.sequence")?
            .create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("prefix", Cell::Str(prefix.into()))
                        .set_cell("padding", Cell::Int(5))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("implementation", Cell::Str("standard".into())),
                ],
            )
            .await?;
        vals.insert("sequence_id".into(), Cell::Id(sequence.ids()[0]));
    }
    Ok(())
}
#[methods(StockPickingType, addon = "stock")]
impl StockPickingType {
    #[method(override)]
    async fn create(
        this: &RecordSet<StockPickingType>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let records = this.as_records().clone();
        tx.savepoint(async move |tx| {
            for vals in &vals_list {
                authorize(&records, tx, vals, Operation::Create).await?;
                crate::operation_type_reservation::validate(vals)?;
            }
            prefetch_warehouses(records.env(), tx, &vals_list).await?;
            let mut cache = LocationCache::default();
            let mut prepared = Vec::with_capacity(vals_list.len());
            for mut vals in vals_list {
                locations(records.env(), tx, &mut vals, &mut cache).await?;
                create_sequence(records.env(), tx, &mut vals).await?;
                prepared.push(vals);
            }
            sup.call(tx, &records, (prepared,)).await
        })
        .await
    }
    #[method(override)]
    async fn copy_data(
        this: &RecordSet<StockPickingType>,
        tx: &mut Transaction,
        defaults: Option<ValueMap>,
        sup: Super,
    ) -> Result<Vec<Option<ValueMap>>> {
        let explicit = defaults
            .as_ref()
            .is_some_and(|v| v.contains_key("sequence_code") || v.contains_key("sequence_id"));
        let mut result: Vec<Option<ValueMap>> =
            sup.call(tx, this.as_records(), (defaults,)).await?;
        if !explicit {
            let prefixes = this.mapped(tx, StockPickingType::SEQUENCE_CODE).await?;
            for (values, prefix) in result.iter_mut().zip(prefixes) {
                if let Some(values) = values
                    && let Some(prefix) = prefix
                {
                    values.insert(
                        "sequence_code".into(),
                        Value::Str(format!("{prefix} (copy)").into()),
                    );
                }
            }
        }
        Ok(result)
    }
    #[compute(depends=["code"])]
    async fn _compute_default_locations(
        this: &RecordSet<StockPickingType>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockPickingType>,
    ) -> Result<()> {
        let companies = this.mapped(tx, StockPickingType::COMPANY_ID).await?;
        let warehouses = this.mapped(tx, StockPickingType::WAREHOUSE_ID).await?;
        let codes = this.mapped(tx, StockPickingType::CODE).await?;
        let mut lookup = Vec::new();
        for id in warehouses.iter().flatten() {
            lookup.push(RawVals::new().set_cell("warehouse_id", Cell::Id(id.get())));
        }
        prefetch_warehouses(this.env(), tx, &lookup).await?;
        let mut cache = LocationCache::default();
        for (i, ((company, warehouse), code)) in
            companies.into_iter().zip(warehouses).zip(codes).enumerate()
        {
            let mut vals = RawVals::new();
            if let Some(company) = company {
                vals.insert("company_id".into(), Cell::Id(company.get()));
            }
            if let Some(warehouse) = warehouse {
                vals.insert("warehouse_id".into(), Cell::Id(warehouse.get()));
            }
            if let Some(code) = code {
                vals.insert("code".into(), Cell::Str(code));
            }
            locations(this.env(), tx, &mut vals, &mut cache).await?;
            for (name, field) in [
                (
                    "default_location_src_id",
                    StockPickingType::DEFAULT_LOCATION_SRC_ID,
                ),
                (
                    "default_location_dest_id",
                    StockPickingType::DEFAULT_LOCATION_DEST_ID,
                ),
            ] {
                let Some(Cell::Id(id)) = vals.get(name) else {
                    return Err(Error::internal(
                        "Operation type default location is missing.",
                    ));
                };
                let id = RecId::from_real(*id).ok_or_else(|| {
                    Error::internal("Operation type default location is invalid.")
                })?;
                out.set(i, field, Some(id))?;
            }
        }
        Ok(())
    }
    #[compute(depends=["favorite_user_ids"], depends_context=["uid"])]
    async fn _compute_is_favorite(
        this: &RecordSet<StockPickingType>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockPickingType>,
    ) -> Result<()> {
        for (i, users) in this
            .mapped(tx, StockPickingType::FAVORITE_USER_IDS)
            .await?
            .iter()
            .enumerate()
        {
            out.set(
                i,
                StockPickingType::IS_FAVORITE,
                users.iter().any(|u| u.get() == this.env().uid()),
            )?;
        }
        Ok(())
    }
    #[method]
    async fn _inverse_is_favorite(
        this: &RecordSet<StockPickingType>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let uid = RecId::from_real(this.env().uid())
            .ok_or_else(|| Error::internal("Favorite actor has no valid user id."))?;
        let flags = this.mapped(tx, StockPickingType::IS_FAVORITE).await?;
        for (record, flag) in this.as_records().iter().zip(flags) {
            let command = if flag {
                Command::Link(uid)
            } else {
                Command::Unlink(uid)
            };
            record
                .write(
                    tx,
                    RawVals::new().set_commands("favorite_user_ids", [command]),
                )
                .await?;
        }
        Ok(())
    }
    #[search_method]
    fn _search_is_favorite(
        req: &FieldSearchRequest<'_>,
        ctx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        use erp_sdk::domain::Value as D;
        match req.operand() {
            D::List(values) if values.iter().any(|value| !matches!(value, D::Bool(_))) => {
                return Err(Error::validation(
                    "is_favorite search requires a boolean equality or boolean membership comparison.",
                ));
            }
            D::Set(values) if values.iter().any(|value| !matches!(value, D::Bool(_))) => {
                return Err(Error::validation(
                    "is_favorite search requires a boolean equality or boolean membership comparison.",
                ));
            }
            _ => {} // scalar validation is operator-specific below
        }
        let matches = |flag| -> Result<bool> {
            match (req.operator(), req.operand()) {
                (Operator::Eq, D::Bool(v)) => Ok(flag == *v),
                (Operator::Ne, D::Bool(v)) => Ok(flag != *v),
                (Operator::In, D::List(v)) => Ok(v.contains(&D::Bool(flag))),
                (Operator::NotIn, D::List(v)) => Ok(!v.contains(&D::Bool(flag))),
                (Operator::In, D::Set(v)) => Ok(v.contains(&D::Bool(flag))),
                (Operator::NotIn, D::Set(v)) => Ok(!v.contains(&D::Bool(flag))),
                _ => Err(Error::validation(
                    "is_favorite search requires a boolean equality or boolean membership comparison.",
                )),
            }
        };
        let member = Domain::field("favorite_user_ids")?.in_(vec![ctx.env().uid()]);
        match (matches(true)?, matches(false)?) {
            (true, true) => Ok(Domain::TRUE),
            (false, false) => Ok(Domain::FALSE),
            (true, false) => Ok(member),
            (false, true) => Ok(!member),
        }
    }
}
pub(crate) async fn write(
    records: &Records,
    tx: &mut Transaction,
    mut vals: RawVals,
    sup: Super<(RawVals,), ()>,
) -> Result<()> {
    if records.is_empty() {
        return sup.call(tx, records, (vals,)).await;
    }
    let records = records.clone();
    tx.savepoint(async move |tx| {
        authorize(&records,tx,&vals,Operation::Write).await?;
        if vals.contains("company_id"){crate::warehouse::check_company_write(&records,tx,&vals).await?;}
        let policy_fields=["reservation_method","reservation_days_before","reservation_days_before_priority"];
        let old_methods=if vals.contains("reservation_method") {
            records.cells(tx,"reservation_method").await?
        } else {Vec::new()};
        // An unrelated edit must not demand read access to reservation configuration.
        if policy_fields.iter().any(|field|vals.contains(field)) {
            for record in records.iter() {
                let mut merged=vals.clone();
                for field in policy_fields {
                    if !merged.contains(field){merged.insert(field.into(),record.cell(tx,field).await?);}
                }
                crate::operation_type_reservation::validate(&merged)?;
            }
        }
        // A prefix edit accompanied by a cleared sequence retains each row's current link,
        // as the sequence-edit contract specifies; no cross-row link is substituted.
        if vals.contains("sequence_code") && matches!(vals.get("sequence_id"),Some(Cell::Null)) {vals.remove("sequence_id");}
        sup.call(tx,&records,(vals.clone(),)).await?;
        for record in records.iter() {
            if let Some(Cell::Str(prefix))=vals.get("sequence_code") {
                let Cell::Id(sid)=record.cell(tx,"sequence_id").await? else {return Err(Error::user("Set a reference sequence before editing an operation type sequence prefix."));};
                let current=RawVals::new().set_cell("warehouse_id",record.cell(tx,"warehouse_id").await?);
                let name=sequence_name(records.env(),tx,&current,prefix).await?;
                records.env().records("ir.sequence")?.browse([sid])?.write(tx,RawVals::new().set_cell("name",Cell::Str(name.into()))).await?;
            }
        }
        for (record,old) in records.iter().zip(old_methods) {
            crate::operation_type_reservation::transition(&record,tx,&old).await?;
        }
        Ok(())
    }).await
}
