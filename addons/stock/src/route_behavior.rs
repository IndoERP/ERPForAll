// SPDX-License-Identifier: MIT OR Apache-2.0
//! Route lifecycle uses caller rights and one savepoint for dependent writes.
use crate::error::StockError;
use crate::route::{StockRoute, StockRule};
use erp_sdk::prelude::*;
use erp_sdk::{Command, Operation};

/// The generated identity of a route or rule (topology ADR D9). Only the warehouse setup that
/// generates the record fills it in, through [`set_generated_identity`].
const GENERATED: [&str; 3] = [
    "generated_warehouse_id",
    "generated_addon",
    "generated_role",
];

fn generated_refusal(records: &Records, field: &str) -> Result<Error> {
    let spec = records.spec();
    let what = if spec.name == "stock.route" {
        "route"
    } else {
        "rule"
    };
    let field = spec
        .field(field)
        .map(|f| f.string.clone())
        .ok_or_else(|| Error::internal(format!("{} has no field '{field}'", spec.name)))?;
    Ok(StockError::GeneratedIdentity { what, field }.into())
}

/// Explicit values and `default_<field>` context keys are refused by name after the create
/// access check; every row then starts without an identity, so no stored default supplies one
/// (the pattern of `move_guards`).
async fn guard_generated_create(
    records: &Records,
    tx: &mut Transaction,
    vals_list: &mut [RawVals],
) -> Result<()> {
    let context = records.env().context();
    let named = GENERATED.iter().copied().find(|f| {
        context.contains(&format!("default_{f}")) || vals_list.iter().any(|v| v.contains(f))
    });
    if let Some(field) = named {
        records.check_access(tx, Operation::Create).await?;
        return Err(generated_refusal(records, field)?);
    }
    for vals in vals_list {
        for field in GENERATED {
            vals.insert(field.into(), Cell::Null);
        }
    }
    Ok(())
}

async fn guard_generated_write(
    records: &Records,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    if let Some(field) = GENERATED.iter().find(|f| vals.contains(f)) {
        records.check_access(tx, Operation::Write).await?;
        return Err(generated_refusal(records, field)?);
    }
    Ok(())
}

/// Records the warehouse, addon and role a generated `stock.route` or `stock.rule` fills (topology
/// ADR D9). This is the one way to set them: the ORM `create` and `write` refuse these fields for
/// every caller, so a warehouse generator creates the record first and then calls this native
/// function, which no RPC call, script or WebAssembly package can reach. The database keeps the
/// three values complete and unique per warehouse, addon and role.
///
/// # Errors
///
/// `Programming` when `record` is not exactly one existing route or rule; the database's
/// refusal when the warehouse already has a record for that addon and role.
pub async fn set_generated_identity(
    tx: &mut Transaction,
    record: &Records,
    warehouse_id: i64,
    addon: &str,
    role: &str,
) -> Result<()> {
    let model = record.spec().name;
    let table = match model {
        "stock.route" => "stock_route",
        "stock.rule" => "stock_rule",
        other => {
            return Err(Error::programming(format!(
                "{other} has no generated route identity."
            )));
        }
    };
    let [id] = record.ids()[..] else {
        return Err(Error::programming(
            "A generated identity is set on exactly one route or rule.",
        ));
    };
    tx.flush().await?;
    let sql = format!(
        "UPDATE public.{table} SET generated_warehouse_id = $1, generated_addon = $2, \
         generated_role = $3 WHERE id = $4"
    );
    if tx
        .execute(&sql, &[&warehouse_id, &addon, &role, &id])
        .await?
        != 1
    {
        return Err(Error::programming(format!(
            "{model} {id} does not exist, so it cannot take a generated identity."
        )));
    }
    tx.invalidate_model(model, Some(&GENERATED)).await
}

// Native constraints use the ORM's integrity environment. Diagnostics retain the outer
// mutation reader. An owned guard also restores the stack if an awaited call is cancelled.
#[derive(Default)]
struct Callers(Arc<std::sync::Mutex<Vec<Env>>>);
struct CallerScope {
    callers: Arc<std::sync::Mutex<Vec<Env>>>,
    depth: usize,
}
impl Drop for CallerScope {
    fn drop(&mut self) {
        match self.callers.lock() {
            Ok(mut callers) => callers.truncate(self.depth),
            // Clear authority during unwinding; the mutex stays poisoned and every later
            // admission/read refuses it rather than recovering a privileged caller.
            Err(mut poison) => poison.get_mut().clear(),
        }
    }
}
async fn with_caller<T>(
    records: &Records,
    tx: &mut Transaction,
    call: impl AsyncFnOnce(&mut Transaction) -> Result<T>,
) -> Result<T> {
    let callers = Arc::clone(&tx.extension::<Callers>().0);
    let depth = {
        let mut stack = callers
            .lock()
            .map_err(|_| Error::programming("Route mutation caller state is poisoned."))?;
        let depth = stack.len();
        stack.push(records.env().clone());
        depth
    };
    let _scope = CallerScope { callers, depth };
    call(tx).await
}

async fn authorize(records: &Records, tx: &mut Transaction, vals: &RawVals) -> Result<()> {
    records.check_access(tx, Operation::Write).await?;
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

async fn refuse_upsert(records: &Records, tx: &mut Transaction) -> Result<erp_sdk::UpsertResult> {
    records.check_access(tx, Operation::Create).await?;
    records.check_access(tx, Operation::Write).await?;
    Err(Error::user(format!(
        "{} cannot be updated by key. Use create or write so route lifecycle validation runs.",
        records.spec().name
    )))
}

/// The outermost caller of the route or rule mutation that is running.
fn mutation_caller(tx: &mut Transaction) -> Result<Env> {
    let callers = Arc::clone(&tx.extension::<Callers>().0);
    callers
        .lock()
        .map_err(|_| Error::programming("Route mutation caller state is poisoned."))?
        .first()
        .cloned()
        .ok_or_else(|| Error::programming("Route company diagnostics require a mutation caller."))
}

/// Company labels are read only after access admission, never under an elevated environment.
async fn mismatch(
    rule: &Records,
    route: &Records,
    tx: &mut Transaction,
    route_check: bool,
) -> Result<Error> {
    let reader = mutation_caller(tx)?;
    let rule = rule.with_env(reader.clone());
    let route = route.with_env(reader);
    let company = rule.related(tx, "company_id").await?;
    let route_company = route.related(tx, "company_id").await?;
    let rule_name = rule.cell(tx, "name").await?;
    let company_name = if company.is_empty() {
        Cell::Str("no company".into())
    } else {
        company.cell(tx, "name").await?
    };
    let route_name = route_company.cell(tx, "name").await?;
    let (Cell::Str(rule_name), Cell::Str(company_name), Cell::Str(route_name)) =
        (rule_name, company_name, route_name)
    else {
        return Err(Error::programming(
            "Route company validation needs readable rule and company names.",
        ));
    };
    let recovery = if route_check {
        "Give the rule and the route the same company."
    } else {
        "Choose a route of the rule's company or of one of its parent companies."
    };
    Ok(Error::validation(format!(
        "The rule \"{rule_name}\" belongs to company {company_name}, but its route belongs to company {route_name}. {recovery}"
    )))
}

#[methods(StockRoute, addon = "stock")]
impl StockRoute {
    #[method(override)]
    async fn create(
        this: &RecordSet<StockRoute>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        guard_generated_create(this.as_records(), tx, &mut vals_list).await?;
        with_caller(this.as_records(), tx, async |tx| {
            sup.call(tx, this.as_records(), (vals_list,)).await
        })
        .await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<StockRoute>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if this.is_empty() {
            return sup.call(tx, this.as_records(), (vals,)).await;
        }
        let records = this.as_records().clone();
        tx.savepoint(async move |tx| {
            authorize(&records, tx, &vals).await?;
            guard_generated_write(&records, tx, &vals).await?;
            for name in ["active", "warehouse_selectable"] {
                if !matches!(vals.get(name), None | Some(Cell::Bool(_))) {
                    return Err(Error::programming(format!(
                        "stock.route.{name} expects true or false."
                    )));
                }
            }
            if matches!(vals.get("warehouse_selectable"), Some(Cell::Bool(false))) {
                // Normalize only after checking the caller's original field assignments.
                if let Some(value) = vals.get("warehouse_ids")
                    && !matches!(value, Cell::Ids(_) | Cell::Null)
                {
                    return Err(Error::programming(
                        "stock.route.warehouse_ids expects a relation id list.",
                    ));
                }
                vals.remove("warehouse_ids");
                vals.insert_commands("warehouse_ids".into(), vec![Command::Clear]);
            }
            let activation = vals.get("active").cloned();
            with_caller(&records, tx, async |tx| {
                sup.call(tx, &records, (vals,)).await
            })
            .await?;
            if let Some(Cell::Bool(active)) = activation {
                let all = records.with_context([("active_test", Value::Bool(false))]);
                let rules = all.related(tx, "rule_ids").await?;
                let destinations = rules.related(tx, "location_dest_id").await?;
                // Prime the prefetch group once, including archived destinations and rules.
                destinations.cells(tx, "active").await?;
                let mut eligible = Vec::new();
                for rule in rules.iter() {
                    let destination = rule.related(tx, "location_dest_id").await?;
                    if destination.cell(tx, "active").await? == Cell::Bool(true) {
                        eligible.extend_from_slice(rule.rec_ids());
                    }
                }
                if !eligible.is_empty() {
                    rules
                        .browse_ids(eligible)
                        .write(tx, RawVals::new().set_cell("active", Cell::Bool(active)))
                        .await?;
                }
            }
            Ok(())
        })
        .await
    }

    /// Archive and restore cascade over the route's rules, so the ORM's integrity environment
    /// lists every rule and the caller must be able to read them all.
    #[constrains("active")]
    async fn _check_route_rules_visible(
        this: &RecordSet<StockRoute>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let rules = this
            .as_records()
            .with_context([("active_test", Value::Bool(false))])
            .related(tx, "rule_ids")
            .await?;
        if rules.is_empty()
            || rules
                .with_env(mutation_caller(tx)?)
                .has_access(tx, Operation::Read)
                .await?
        {
            return Ok(());
        }
        Err(Error::access(
            "This route has rules of companies you cannot access, so it cannot be archived or \
             restored here. Ask a user with access to all of its companies to do it.",
        ))
    }

    #[constrains("company_id")]
    async fn _check_route_company(
        this: &RecordSet<StockRoute>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let all = this
            .as_records()
            .with_context([("active_test", Value::Bool(false))]);
        for route in all.iter() {
            let company = route.cell(tx, "company_id").await?;
            if matches!(company, Cell::Null) {
                continue;
            }
            let rules = route.related(tx, "rule_ids").await?;
            let companies = rules.cells(tx, "company_id").await?;
            for (rule, rule_company) in rules.iter().zip(companies) {
                if rule_company != company {
                    return Err(mismatch(&rule, &route, tx, true).await?);
                }
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn upsert(
        this: &RecordSet<StockRoute>,
        tx: &mut Transaction,
        _call: erp_sdk::UpsertCall,
        _sup: Super,
    ) -> Result<erp_sdk::UpsertResult> {
        refuse_upsert(this.as_records(), tx).await
    }

    /// Deleting a route deletes its rules through the database cascade, where no constraint
    /// runs. So, after the delete access check, the routes are locked (a concurrent rule insert
    /// waits) and their rules are listed by SQL, every company included, the way the tracking
    /// guard reads stock rows; the caller must be able to read them all.
    #[method(override)]
    async fn unlink(this: &RecordSet<StockRoute>, tx: &mut Transaction, sup: Super) -> Result<()> {
        let records = this.as_records();
        if records.is_empty() {
            return sup.call(tx, records, ()).await;
        }
        records.check_access(tx, Operation::Unlink).await?;
        let ids = records.ids();
        tx.flush().await?;
        crate::quant_guards::lock_guarded(
            tx,
            "SELECT FROM public.stock_route WHERE id = ANY($1) ORDER BY id FOR UPDATE NOWAIT",
            &ids,
            "routes",
        )
        .await?;
        let mut rule_ids = Vec::new();
        for row in tx
            .query(
                "SELECT id FROM public.stock_rule WHERE route_id = ANY($1)",
                &[&ids],
            )
            .await?
        {
            rule_ids.push(
                row.try_get::<_, i64>(0)
                    .map_err(|e| Error::internal(format!("route rule read: {e}")))?,
            );
        }
        if !rule_ids.is_empty()
            && !records
                .env()
                .records("stock.rule")?
                .browse(rule_ids)?
                .has_access(tx, Operation::Read)
                .await?
        {
            return Err(Error::access(
                "This route has rules of companies you cannot access, so it cannot be deleted \
                 here. Ask a user with access to all of its companies to do it.",
            ));
        }
        sup.call(tx, records, ()).await
    }
}

#[methods(StockRule, addon = "stock")]
impl StockRule {
    #[method(override)]
    async fn create(
        this: &RecordSet<StockRule>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        guard_generated_create(this.as_records(), tx, &mut vals_list).await?;
        with_caller(this.as_records(), tx, async |tx| {
            sup.call(tx, this.as_records(), (vals_list,)).await
        })
        .await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<StockRule>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        guard_generated_write(this.as_records(), tx, &vals).await?;
        with_caller(this.as_records(), tx, async |tx| {
            sup.call(tx, this.as_records(), (vals,)).await
        })
        .await
    }

    /// Replaces only route_id's generic exact-company predicate; all other link checks remain.
    #[constrains("company_id", "route_id")]
    async fn _check_rule_route_company(
        this: &RecordSet<StockRule>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for rule in this.as_records().iter() {
            let route = rule.related(tx, "route_id").await?;
            let route_company = route.cell(tx, "company_id").await?;
            if matches!(route_company, Cell::Null) {
                continue;
            }
            let company = rule.related(tx, "company_id").await?;
            let Cell::Id(route_company_id) = route_company else {
                return Err(Error::programming("A route company must be a record id."));
            };
            if company.is_empty() {
                return Err(mismatch(&rule, &route, tx, false).await?);
            }
            let path = company.cell(tx, "parent_path").await?;
            let Cell::Str(path) = path else {
                return Err(Error::programming(
                    "Rule company validation needs its company hierarchy path.",
                ));
            };
            let Some(body) = path.strip_suffix('/') else {
                return Err(Error::programming(
                    "Rule company hierarchy path must end in a slash.",
                ));
            };
            let mut matches = false;
            for ancestor in body.split('/') {
                let id = ancestor.parse::<i64>().map_err(|_| {
                    Error::programming("Rule company hierarchy path contains an invalid id.")
                })?;
                if id <= 0 {
                    return Err(Error::programming(
                        "Rule company hierarchy path contains a nonpositive id.",
                    ));
                }
                matches |= id == route_company_id;
            }
            if !matches {
                return Err(mismatch(&rule, &route, tx, false).await?);
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn upsert(
        this: &RecordSet<StockRule>,
        tx: &mut Transaction,
        _call: erp_sdk::UpsertCall,
        _sup: Super,
    ) -> Result<erp_sdk::UpsertResult> {
        refuse_upsert(this.as_records(), tx).await
    }
}
