// SPDX-License-Identifier: MIT OR Apache-2.0
//! Route lifecycle uses caller rights and one savepoint for dependent writes.
use crate::route::{StockRoute, StockRule};
use erp_sdk::prelude::*;
use erp_sdk::{Command, Operation};

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
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
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
}

#[methods(StockRule, addon = "stock")]
impl StockRule {
    #[method(override)]
    async fn create(
        this: &RecordSet<StockRule>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
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
