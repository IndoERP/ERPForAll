// SPDX-License-Identifier: MIT OR Apache-2.0
//! The reconciliation service: match, unmatch, and remove a settlement, each as one unit with the
//! locks.1 taken in order (entry headers by id, then open items by line id, then
//! matches and settlements by id), set-based statements, and the invoice status written in the same
//! unit by one function ([`refresh_moves`], "Payment and invoice state").

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use super::RECONCILER_GROUP;
use super::plan::{self, Open, Pair, Settlement};

/// The most journal items one interactive reconciliation may reach through its matches (
/// D5 interactive ceiling; a safety limit, not a setting).
const MAX_ITEMS: usize = 2_000;
/// The most matches one interactive reconciliation may reach.
const MAX_MATCHES: usize = 4_000;

const INVOICE_TYPES: [&str; 4] = ["out_invoice", "out_refund", "in_invoice", "in_refund"];

fn db(error: impl std::fmt::Display) -> Error {
    Error::db(format!(
        "A journal item could not be read for reconciliation: {error}"
    ))
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Asks the caller's reconciliation right: membership of [`RECONCILER_GROUP`] and `operation` on
/// `records`, before any statement reads or locks a caller-named record.
async fn authorize(tx: &mut Transaction, records: &Records, operation: Operation) -> Result<()> {
    let env = records.env().clone();
    if !env.su() && !tx.has_group(&env, RECONCILER_GROUP).await? {
        return Err(Error::access(format!(
            "Only accounting users may match and unmatch journal items ({RECONCILER_GROUP})."
        )));
    }
    records.check_access(tx, operation).await
}

/// What reconciliation reads of one journal item, after its locks.
#[derive(Debug)]
struct Item {
    id: i64,
    move_id: i64,
    move_name: Option<String>,
    parent_state: String,
    account: Option<i64>,
    reconcile: bool,
    company: i64,
    root: i64,
    currency: i64,
    company_currency: i64,
    balance: Decimal,
    amount_currency: Decimal,
    date: Date,
    due: Option<Date>,
    partner: Option<i64>,
    full: Option<i64>,
    open: Option<(Decimal, Decimal, i64)>,
}

impl Item {
    /// What is still due in the company currency.
    fn residual(&self) -> Decimal {
        match (self.open, self.full) {
            (Some((residual, _, _)), _) => residual,
            (None, Some(_)) => Decimal::ZERO,
            (None, None) => self.balance,
        }
    }

    fn residual_currency(&self) -> Decimal {
        match (self.open, self.full) {
            (Some((_, residual, _)), _) => residual,
            (None, Some(_)) => Decimal::ZERO,
            (None, None) => self.amount_currency,
        }
    }

    /// Both residuals are zero (the reference's `reconciled`).
    fn settled(&self) -> bool {
        self.residual().is_zero() && self.residual_currency().is_zero()
    }

    fn component(&self) -> Option<i64> {
        self.open.map(|(_, _, component)| component)
    }

    fn entry(&self) -> String {
        self.move_name
            .clone()
            .unwrap_or_else(|| format!("draft entry {}", self.move_id))
    }
}

async fn read_items(tx: &mut Transaction, ids: &[i64]) -> Result<BTreeMap<i64, Item>> {
    let rows = tx
        .query(
            "SELECT l.id::int8, l.move_id::int8, m.name::text, l.parent_state::text, \
             l.account_id::int8, COALESCE(a.reconcile, false), l.company_id::int8, \
             NULLIF(split_part(co.parent_path, '/', 1), '')::int8, l.currency_id::int8, \
             co.currency_id::int8, COALESCE(l.balance, 0), COALESCE(l.amount_currency, 0), l.date, \
             l.date_maturity, l.partner_id::int8, l.full_reconcile_id::int8, o.amount_residual, \
             o.amount_residual_currency, o.component_id::int8 \
             FROM account_move_line l \
             JOIN account_move m ON m.id = l.move_id \
             JOIN res_company co ON co.id = l.company_id \
             LEFT JOIN account_account a ON a.id = l.account_id \
             LEFT JOIN account_open_item o ON o.line_id = l.id \
             WHERE l.id = ANY($1::int8[]) ORDER BY l.id",
            &[&ids],
        )
        .await?;
    if rows.len() != ids.len() {
        return Err(Error::missing("Some of the journal items no longer exist."));
    }
    let mut items = BTreeMap::new();
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(db)?;
        let root: Option<i64> = row.try_get(7).map_err(db)?;
        let residual: Option<Decimal> = row.try_get(16).map_err(db)?;
        let residual_currency: Option<Decimal> = row.try_get(17).map_err(db)?;
        let component: Option<i64> = row.try_get(18).map_err(db)?;
        let open = match (residual, residual_currency, component) {
            (Some(r), Some(c), Some(label)) => Some((r, c, label)),
            (None, None, None) => None,
            _ => {
                return Err(Error::invariant(format!(
                    "The open-item row of journal item {id} is incomplete."
                )));
            }
        };
        let date: Option<Date> = row.try_get(12).map_err(db)?;
        items.insert(
            id,
            Item {
                id,
                move_id: row.try_get(1).map_err(db)?,
                move_name: row.try_get(2).map_err(db)?,
                parent_state: row.try_get(3).map_err(db)?,
                account: row.try_get(4).map_err(db)?,
                reconcile: row.try_get(5).map_err(db)?,
                company: row.try_get(6).map_err(db)?,
                root: root.ok_or_else(|| {
                    Error::internal(format!("The company of journal item {id} has no path."))
                })?,
                currency: row.try_get(8).map_err(db)?,
                company_currency: row.try_get(9).map_err(db)?,
                balance: row.try_get(10).map_err(db)?,
                amount_currency: row.try_get(11).map_err(db)?,
                date: date.ok_or_else(|| {
                    Error::internal(format!("Journal item {id} has no accounting date."))
                })?,
                due: row.try_get(13).map_err(db)?,
                partner: row.try_get(14).map_err(db)?,
                full: row.try_get(15).map_err(db)?,
                open,
            },
        );
    }
    Ok(items)
}

/// The journal items connected to some seed items through matches, and those matches.
#[derive(Debug, Default)]
struct Graph {
    lines: BTreeSet<i64>,
    edges: BTreeMap<i64, (i64, i64)>,
}

impl Graph {
    /// The connected groups of matched items: (items, matches); items without a match are left out.
    fn components(&self) -> Vec<(Vec<i64>, Vec<i64>)> {
        let mut parent: BTreeMap<i64, i64> = BTreeMap::new();
        fn find(parent: &mut BTreeMap<i64, i64>, x: i64) -> i64 {
            let mut root = x;
            while let Some(&p) = parent.get(&root) {
                if p == root {
                    break;
                }
                root = p;
            }
            parent.insert(x, root);
            root
        }
        for &(debit, credit) in self.edges.values() {
            parent.entry(debit).or_insert(debit);
            parent.entry(credit).or_insert(credit);
            let (a, b) = (find(&mut parent, debit), find(&mut parent, credit));
            if a != b {
                parent.insert(a.max(b), a.min(b));
            }
        }
        let mut groups: BTreeMap<i64, (Vec<i64>, Vec<i64>)> = BTreeMap::new();
        let vertices: Vec<i64> = parent.keys().copied().collect();
        for line in vertices {
            let root = find(&mut parent, line);
            groups.entry(root).or_default().0.push(line);
        }
        for (&partial, &(debit, _)) in &self.edges {
            let root = find(&mut parent, debit);
            groups.entry(root).or_default().1.push(partial);
        }
        groups.into_values().collect()
    }

    /// The other items each item is matched with.
    fn counterparts(&self, line: i64) -> impl Iterator<Item = i64> + '_ {
        self.edges.values().filter_map(move |&(d, c)| {
            if d == line {
                Some(c)
            } else if c == line {
                Some(d)
            } else {
                None
            }
        })
    }
}

/// Walks the matches from `seeds` to every connected item, within the interactive ceilings.
async fn closure(tx: &mut Transaction, seeds: &[i64]) -> Result<Graph> {
    let mut graph = Graph::default();
    let mut frontier: Vec<i64> = seeds.to_vec();
    graph.lines.extend(seeds.iter().copied());
    while !frontier.is_empty() {
        let rows = tx
            .query(
                "SELECT id::int8, debit_move_id::int8, credit_move_id::int8 \
                 FROM account_partial_reconcile \
                 WHERE debit_move_id = ANY($1::int8[]) OR credit_move_id = ANY($1::int8[])",
                &[&frontier],
            )
            .await?;
        let mut next = Vec::new();
        for row in &rows {
            let id: i64 = row.try_get(0).map_err(db)?;
            let debit: i64 = row.try_get(1).map_err(db)?;
            let credit: i64 = row.try_get(2).map_err(db)?;
            if graph.edges.insert(id, (debit, credit)).is_none() {
                for line in [debit, credit] {
                    if graph.lines.insert(line) {
                        next.push(line);
                    }
                }
            }
        }
        if graph.lines.len() > MAX_ITEMS || graph.edges.len() > MAX_MATCHES {
            return Err(Error::user(format!(
                "These journal items are linked through their matches to more than {MAX_ITEMS} \
                 items or {MAX_MATCHES} matches, more than one interactive reconciliation may \
                 change at once."
            )));
        }
        frontier = next;
    }
    Ok(graph)
}

/// Takes the locks of a reconciliation unit over `lines` in the order.1: their entry
/// headers by id with a version touch (a concurrent writer of one of them conflicts), then their
/// open items by line id, then the matches and settlements they hold by id.
async fn hold(tx: &mut Transaction, graph: &Graph) -> Result<()> {
    let lines: Vec<i64> = graph.lines.iter().copied().collect();
    tx.execute(
        "WITH held AS (SELECT id FROM account_move WHERE id IN \
                       (SELECT move_id FROM account_move_line WHERE id = ANY($1::int8[])) \
                       ORDER BY id FOR NO KEY UPDATE) \
         UPDATE account_move m SET write_date = m.write_date FROM held WHERE m.id = held.id",
        &[&lines],
    )
    .await?;
    tx.execute(
        "SELECT line_id FROM account_open_item WHERE line_id = ANY($1::int8[]) \
         ORDER BY line_id FOR UPDATE",
        &[&lines],
    )
    .await?;
    let partials: Vec<i64> = graph.edges.keys().copied().collect();
    tx.execute(
        "SELECT id FROM account_partial_reconcile WHERE id = ANY($1::int8[]) ORDER BY id FOR UPDATE",
        &[&partials],
    )
    .await?;
    tx.execute(
        "SELECT id FROM account_full_reconcile WHERE id IN \
         (SELECT full_reconcile_id FROM account_partial_reconcile WHERE id = ANY($1::int8[])) \
         ORDER BY id FOR UPDATE",
        &[&partials],
    )
    .await?;
    Ok(())
}

/// Matches the debit and credit items of `lines` with each other as far as they offset
/// (`reconcile`): matches are created, residuals
/// lowered by their amounts, components relabelled, settled components get their settlement, and the
/// invoices among the entries get their amount due and payment status.
///
/// # Errors
///
/// `Access` without [`RECONCILER_GROUP`] or write access to the items; `UserError` naming what
/// cannot be matched (a settled item and the entry that settles it, a draft or cancelled entry,
/// different accounts or root companies, an account without reconciliation, another currency than
/// the company's); `Conflict` or a serialization failure when a concurrent unit changed the items.
pub async fn reconcile_lines(tx: &mut Transaction, lines: &Records) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    authorize(tx, lines, Operation::Write).await?;
    let ids = sorted(lines.ids());
    let env = lines.env().clone();
    tx.savepoint(async |tx| {
        tx.flush().await?;
        let graph = closure(tx, &ids).await?;
        hold(tx, &graph).await?;
        let all: Vec<i64> = graph.lines.iter().copied().collect();
        let items = read_items(tx, &all).await?;
        let chosen = admit(tx, &env, &ids, &items, &graph).await?;
        let pairs = plan::pair(
            chosen
                .iter()
                .map(|item| Open {
                    id: item.id,
                    residual: item.residual(),
                    due: item.due.unwrap_or(item.date),
                    amount_currency: item.amount_currency,
                    balance: item.balance,
                    partner: item.partner,
                })
                .collect(),
        );
        if pairs.is_empty() {
            return Ok(());
        }
        let endpoints = insert_matches(tx, &pairs, &items).await?;
        let graph = closure(tx, &endpoints).await?;
        settle(tx, &graph, true).await?;
        let moves: Vec<i64> = sorted(
            endpoints
                .iter()
                .filter_map(|l| items.get(l))
                .map(|i| i.move_id)
                .collect(),
        );
        refresh_moves(tx, &moves).await?;
        invalidate(tx).await
    })
    .await
}

/// The selected items that take part, after the checks: a settled item whose component also has an
/// open item in the selection is left out; any other settled item is refused.
async fn admit<'a>(
    tx: &mut Transaction,
    env: &Env,
    ids: &[i64],
    items: &'a BTreeMap<i64, Item>,
    graph: &Graph,
) -> Result<Vec<&'a Item>> {
    let selected: Vec<&Item> = ids.iter().filter_map(|id| items.get(id)).collect();
    if let Some(item) = selected.iter().find(|i| i.parent_state == "cancel") {
        return Err(Error::user(format!(
            "The items of cancelled entries cannot be matched ({}).",
            item.entry()
        )));
    }
    if let Some(item) = selected.iter().find(|i| i.parent_state != "posted") {
        return Err(Error::user(format!(
            "Only the items of posted entries can be matched; post {} first.",
            item.entry()
        )));
    }
    let open_components: BTreeSet<i64> = selected
        .iter()
        .filter(|i| !i.settled())
        .filter_map(|i| i.component())
        .collect();
    let chosen: Vec<&Item> = selected
        .into_iter()
        .filter(|i| !i.settled() || i.component().is_none_or(|c| !open_components.contains(&c)))
        .collect();
    // a never-matched item without an amount reads as settled, but nothing settled it
    if let Some(item) = chosen.iter().find(|i| {
        i.full.is_none() && i.open.is_none() && i.balance.is_zero() && i.amount_currency.is_zero()
    }) {
        return Err(Error::user(format!(
            "The item of {} has no amount to match.",
            item.entry()
        )));
    }
    if let Some(item) = chosen.iter().find(|i| i.settled()) {
        // the counterparts come from the match walk, not the selection: name only those the
        // caller may read, count the rest
        let blocking: BTreeMap<i64, String> = graph
            .counterparts(item.id)
            .filter_map(|other| items.get(&other))
            .map(|other| (other.move_id, other.entry()))
            .collect();
        let readable = env
            .sudo(false)
            .records("account.move")?
            .browse(blocking.keys().copied())?
            .filtered_access(tx, Operation::Read)
            .await?
            .ids();
        let mut named: Vec<String> = readable
            .iter()
            .filter_map(|id| blocking.get(id).cloned())
            .collect();
        named.sort_unstable();
        named.dedup();
        match blocking.len().saturating_sub(readable.len()) {
            0 => {}
            1 => named.push("1 entry you cannot open".to_owned()),
            hidden => named.push(format!("{hidden} entries you cannot open")),
        }
        let by = if named.is_empty() {
            String::new()
        } else {
            format!(" It is already matched by {}.", named.join(", "))
        };
        return Err(Error::user(format!(
            "Some of these journal items are already fully matched: the item of {}.{by}",
            item.entry()
        )));
    }
    let accounts: BTreeSet<Option<i64>> = chosen.iter().map(|i| i.account).collect();
    let account_ids: Vec<i64> = accounts.iter().flatten().copied().collect();
    if accounts.contains(&None) {
        return Err(Error::user(
            "A section or note line has no account and cannot be matched.",
        ));
    }
    let names = display_names(tx, env, "account.account", &account_ids).await?;
    if let Some(item) = chosen.iter().find(|i| !i.reconcile) {
        let name = item
            .account
            .and_then(|a| names.get(&a))
            .cloned()
            .unwrap_or_default();
        return Err(Error::user(format!(
            "The account {name} does not allow reconciliation; only items on such accounts \
             (receivable and payable accounts among them) can be matched."
        )));
    }
    if account_ids.len() > 1 {
        return Err(Error::user(format!(
            "Only items of one account can be matched together; these are on {}.",
            names.into_values().collect::<Vec<_>>().join(", ")
        )));
    }
    let roots: BTreeSet<i64> = chosen.iter().map(|i| i.root).collect();
    if roots.len() > 1 {
        let companies: Vec<i64> = sorted(chosen.iter().map(|i| i.company).collect());
        let names = display_names(tx, env, "res.company", &companies).await?;
        return Err(Error::user(format!(
            "Only items of one company and its branches can be matched together; these belong to \
             {}.",
            names.into_values().collect::<Vec<_>>().join(", ")
        )));
    }
    let foreign_currencies: BTreeSet<i64> = chosen
        .iter()
        .filter(|i| i.currency != i.company_currency)
        .map(|i| i.currency)
        .collect();
    if foreign_currencies.len() > 1 {
        return Err(Error::user(
            "Items in two different foreign currencies cannot be matched directly together.",
        ));
    }
    Ok(chosen)
}

/// Pure exact-decimal computation of the currency amount settled on one side of a partial match.
/// When the line is in company currency, returns `matched_company`. When in foreign currency,
/// settles the full remaining currency residual if `matched_company >= remaining_company.abs()`,
/// otherwise allocates proportionally rounded to `scale` decimal places.
pub fn settled_currency_amount(
    matched_company: Decimal,
    remaining_company: Decimal,
    remaining_currency: Decimal,
    same_as_company_currency: bool,
    scale: u32,
) -> Decimal {
    if same_as_company_currency {
        return matched_company;
    }
    let rem_co_abs = remaining_company.abs();
    let rem_cur_abs = remaining_currency.abs();
    if rem_co_abs.is_zero() || rem_cur_abs.is_zero() {
        return Decimal::ZERO;
    }
    if matched_company >= rem_co_abs {
        rem_cur_abs
    } else {
        ((rem_cur_abs * matched_company) / rem_co_abs).round_dp(scale)
    }
}

/// Pure exact-decimal foreign-exchange gain/loss between a debit and credit settling the same
/// foreign currency amount (`debit_company - credit_company`; positive = FX loss on receivable,
/// negative = FX gain).
pub fn compute_fx_difference(debit_company: Decimal, credit_company: Decimal) -> Decimal {
    debit_company - credit_company
}

/// The display names of `ids` in `model`, read with the caller's rights.
async fn display_names(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: &[i64],
) -> Result<BTreeMap<i64, String>> {
    let records = env.records(model)?.browse(ids.iter().copied())?;
    let cells = records.cells(tx, "display_name").await?;
    Ok(records
        .ids()
        .into_iter()
        .zip(cells)
        .map(|(id, cell)| match cell {
            Cell::Str(name) => (id, name.to_string()),
            _ => (id, format!("#{id}")),
        })
        .collect())
}

/// Inserts the matches of `pairs` and lowers the residuals of their items by the matched amounts
/// in one grouped update; an item's first match creates its open-item row from
/// its amounts. Returns the matched items, sorted.
async fn insert_matches(
    tx: &mut Transaction,
    pairs: &[Pair],
    items: &BTreeMap<i64, Item>,
) -> Result<Vec<i64>> {
    let debits: Vec<i64> = pairs.iter().map(|p| p.debit).collect();
    let credits: Vec<i64> = pairs.iter().map(|p| p.credit).collect();
    let amounts: Vec<Decimal> = pairs.iter().map(|p| p.amount).collect();
    let mut rem_co: BTreeMap<i64, Decimal> = items
        .iter()
        .map(|(&id, item)| (id, item.residual().abs()))
        .collect();
    let mut rem_cur: BTreeMap<i64, Decimal> = items
        .iter()
        .map(|(&id, item)| (id, item.residual_currency().abs()))
        .collect();
    let mut cur_ids: Vec<RecId> = items
        .values()
        .flat_map(|i| [i.currency, i.company_currency])
        .filter_map(RecId::from_real)
        .collect();
    cur_ids.sort_unstable();
    cur_ids.dedup();
    let rounding = if cur_ids.is_empty() {
        None
    } else {
        Some(erp_addon_base::money::RoundingTable::load(&tx.env(), tx, &cur_ids).await?)
    };
    let scale_of = |cur_id: i64| -> Result<u32> {
        let Some(ref table) = rounding else {
            return Ok(2);
        };
        let rec = RecId::from_real(cur_id)
            .ok_or_else(|| Error::invariant(format!("Invalid currency id {cur_id}.")))?;
        Ok(table.get(rec)?.decimal_places())
    };
    let mut debit_curs = Vec::with_capacity(pairs.len());
    let mut credit_curs = Vec::with_capacity(pairs.len());
    for p in pairs {
        let d_item = items.get(&p.debit);
        let c_item = items.get(&p.credit);
        let d_same = d_item.is_none_or(|i| i.currency == i.company_currency);
        let c_same = c_item.is_none_or(|i| i.currency == i.company_currency);
        let d_scale = match d_item {
            Some(i) => scale_of(i.currency)?,
            None => 2,
        };
        let c_scale = match c_item {
            Some(i) => scale_of(i.currency)?,
            None => 2,
        };
        let d_rco = rem_co.get(&p.debit).copied().unwrap_or(p.amount);
        let d_rcur = rem_cur.get(&p.debit).copied().unwrap_or(p.amount);
        let d_amt_cur = settled_currency_amount(p.amount, d_rco, d_rcur, d_same, d_scale);
        rem_co.insert(p.debit, (d_rco - p.amount).max(Decimal::ZERO));
        rem_cur.insert(p.debit, (d_rcur - d_amt_cur).max(Decimal::ZERO));
        debit_curs.push(d_amt_cur);

        let c_rco = rem_co.get(&p.credit).copied().unwrap_or(p.amount);
        let c_rcur = rem_cur.get(&p.credit).copied().unwrap_or(p.amount);
        let c_amt_cur = settled_currency_amount(p.amount, c_rco, c_rcur, c_same, c_scale);
        rem_co.insert(p.credit, (c_rco - p.amount).max(Decimal::ZERO));
        rem_cur.insert(p.credit, (c_rcur - c_amt_cur).max(Decimal::ZERO));
        credit_curs.push(c_amt_cur);
    }
    let endpoints = sorted(debits.iter().chain(&credits).copied().collect());
    let uid = tx.env().uid();
    tx.execute(
        "INSERT INTO account_partial_reconcile (debit_move_id, credit_move_id, company_id, \
         debit_currency_id, credit_currency_id, amount, debit_amount_currency, \
         credit_amount_currency, max_date, create_uid, create_date, write_uid, write_date) \
         SELECT v.d, v.c, dl.company_id, dl.currency_id, cl.currency_id, v.a, v.da, v.ca, \
                GREATEST(dl.date, cl.date), $6, now() AT TIME ZONE 'UTC', $6, now() AT TIME ZONE 'UTC' \
         FROM unnest($1::int8[], $2::int8[], $3::numeric[], $4::numeric[], $5::numeric[]) \
              WITH ORDINALITY AS v(d, c, a, da, ca, n) \
         JOIN account_move_line dl ON dl.id = v.d JOIN account_move_line cl ON cl.id = v.c \
         ORDER BY v.n",
        &[&debits, &credits, &amounts, &debit_curs, &credit_curs, &uid],
    )
    .await?;
    // a concurrent first match of the same item is a unique conflict or a serialization failure
    tx.execute(
        "INSERT INTO account_open_item (line_id, company_id, account_id, partner_id, currency_id, \
         balance, amount_currency, amount_residual, amount_residual_currency, component_id, \
         create_uid, create_date, write_uid, write_date) \
         SELECT l.id, l.company_id, l.account_id, l.partner_id, l.currency_id, \
                COALESCE(l.balance, 0), COALESCE(l.amount_currency, 0), COALESCE(l.balance, 0), \
                COALESCE(l.amount_currency, 0), 0, \
                $2, now() AT TIME ZONE 'UTC', $2, now() AT TIME ZONE 'UTC' \
         FROM account_move_line l WHERE l.id = ANY($1::int8[]) \
         AND NOT EXISTS (SELECT 1 FROM account_open_item o WHERE o.line_id = l.id) ORDER BY l.id",
        &[&endpoints, &uid],
    )
    .await?;
    let rows = tx
        .query(
            "UPDATE account_open_item o SET \
             amount_residual = o.amount_residual - d.delta, \
             amount_residual_currency = o.amount_residual_currency - d.delta_cur, \
             write_uid = $6, write_date = now() AT TIME ZONE 'UTC' \
             FROM (SELECT line_id, SUM(delta) AS delta, SUM(delta_cur) AS delta_cur FROM ( \
                     SELECT x.line_id, x.delta, x.delta_cur \
                     FROM unnest($1::int8[], $3::numeric[], $4::numeric[]) AS x(line_id, delta, delta_cur) \
                     UNION ALL \
                     SELECT y.line_id, -y.delta, -y.delta_cur \
                     FROM unnest($2::int8[], $3::numeric[], $5::numeric[]) AS y(line_id, delta, delta_cur) \
                   ) s GROUP BY line_id) d \
             WHERE o.line_id = d.line_id RETURNING o.line_id::int8",
            &[&debits, &credits, &amounts, &debit_curs, &credit_curs, &uid],
        )
        .await?;
    if rows.len() != endpoints.len() {
        return Err(Error::conflict(
            "A journal item was matched or settled by someone else meanwhile; reload and try again.",
        ));
    }
    Ok(endpoints)
}

/// Labels every component of `graph` with its smallest match id; with `close`, a component whose
/// items are all settled gets its settlement and loses its open-item rows (
/// -129). Set-based: one relabel, one settlement insert, one write per table.
async fn settle(tx: &mut Transaction, graph: &Graph, close: bool) -> Result<()> {
    let components = graph.components();
    let mut lines = Vec::new();
    let mut labels = Vec::new();
    for (members, partials) in &components {
        let label =
            partials.iter().min().copied().ok_or_else(|| {
                Error::invariant("A group of matched journal items has no match.")
            })?;
        let label = i32::try_from(label)
            .map_err(|_| Error::invariant(format!("Match id {label} is out of range.")))?;
        for line in members {
            lines.push(*line);
            labels.push(label);
        }
    }
    tx.execute(
        "UPDATE account_open_item o SET component_id = v.label \
         FROM unnest($1::int8[], $2::int4[]) AS v(line_id, label) \
         WHERE o.line_id = v.line_id AND o.component_id <> v.label",
        &[&lines, &labels],
    )
    .await?;
    if !close {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT line_id::int8 FROM account_open_item WHERE line_id = ANY($1::int8[]) \
             AND amount_residual = 0 AND amount_residual_currency = 0",
            &[&lines],
        )
        .await?;
    let zero: BTreeSet<i64> = rows
        .iter()
        .map(|row| row.try_get::<_, i64>(0).map_err(db))
        .collect::<Result<_>>()?;
    let full: Vec<&(Vec<i64>, Vec<i64>)> = components
        .iter()
        .filter(|(members, _)| members.iter().all(|line| zero.contains(line)))
        .collect();
    if full.is_empty() {
        return Ok(());
    }
    let count = i64::try_from(full.len()).map_err(|_| Error::invariant("too many settlements"))?;
    let ids: Vec<i64> = tx
        .query(
            "SELECT nextval(pg_get_serial_sequence('account_full_reconcile', 'id'))::int8 \
             FROM generate_series(1, $1::int8)",
            &[&count],
        )
        .await?
        .iter()
        .map(|row| row.try_get::<_, i64>(0).map_err(db))
        .collect::<Result<_>>()?;
    let (mut set_lines, mut line_fulls, mut set_partials, mut partial_fulls, mut anchors) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ((members, partials), id) in full.iter().zip(&ids) {
        anchors.push(members.first().copied().unwrap_or_default());
        for line in members {
            set_lines.push(*line);
            line_fulls.push(*id);
        }
        for partial in partials {
            set_partials.push(*partial);
            partial_fulls.push(*id);
        }
    }
    let uid = tx.env().uid();
    tx.execute(
        "INSERT INTO account_full_reconcile (id, company_id, create_uid, create_date, write_uid, \
         write_date) \
         SELECT v.id, l.company_id, $3, now() AT TIME ZONE 'UTC', $3, now() AT TIME ZONE 'UTC' \
         FROM unnest($1::int8[], $2::int8[]) AS v(id, line_id) \
         JOIN account_move_line l ON l.id = v.line_id ORDER BY v.id",
        &[&ids, &anchors, &uid],
    )
    .await?;
    tx.execute(
        "UPDATE account_move_line l SET full_reconcile_id = v.full_id \
         FROM unnest($1::int8[], $2::int8[]) AS v(line_id, full_id) WHERE l.id = v.line_id",
        &[&set_lines, &line_fulls],
    )
    .await?;
    tx.execute(
        "UPDATE account_partial_reconcile p SET full_reconcile_id = v.full_id \
         FROM unnest($1::int8[], $2::int8[]) AS v(id, full_id) WHERE p.id = v.id",
        &[&set_partials, &partial_fulls],
    )
    .await?;
    tx.execute(
        "DELETE FROM account_open_item WHERE line_id = ANY($1::int8[])",
        &[&set_lines],
    )
    .await?;
    Ok(())
}

/// Removes the matches `partials` (`unlink`, `remove_move_reconcile`): the
/// settlements of their components go, residuals are rebuilt from the matches that remain, the
/// remaining components are relabelled and the invoices among the entries get their status back.
///
/// # Errors
///
/// `Access` without [`RECONCILER_GROUP`], the right to delete the matches (as the reference asks:
/// unmatching deletes them, from either entry point) or write access to their items;
/// `Validation` when the entry of an item of the match walk is dated in a locked period (the lock
/// dates of [`crate::moves::refuse_locked_items`]);
/// `Missing` for a match that no longer exists; serialization failures as [`reconcile_lines`].
pub async fn unreconcile_partials(tx: &mut Transaction, partials: &Records) -> Result<()> {
    if partials.is_empty() {
        return Ok(());
    }
    authorize(tx, partials, Operation::Unlink).await?;
    let ids = sorted(partials.ids());
    let env = partials.env().clone();
    tx.savepoint(async |tx| {
        tx.flush().await?;
        let rows = tx
            .query(
                "SELECT p.debit_move_id::int8, p.credit_move_id::int8, dl.company_id::int8, cl.company_id::int8 \
                 FROM account_partial_reconcile p JOIN account_move_line dl ON dl.id = p.debit_move_id \
                 JOIN account_move_line cl ON cl.id = p.credit_move_id WHERE p.id = ANY($1::int8[])",
                &[&ids],
            )
            .await?;
        if rows.len() != ids.len() {
            return Err(Error::missing("Some of the matches no longer exist."));
        }
        let mut endpoints = Vec::new();
        let mut companies = BTreeSet::new();
        for row in &rows {
            endpoints.push(row.try_get::<_, i64>(0).map_err(db)?);
            endpoints.push(row.try_get::<_, i64>(1).map_err(db)?);
            companies.insert(row.try_get::<_, i64>(2).map_err(db)?);
            companies.insert(row.try_get::<_, i64>(3).map_err(db)?);
        }
        let endpoints = sorted(endpoints);
        env.records("account.move.line")?
            .browse(endpoints.iter().copied())?
            .check_access(tx, Operation::Write)
            .await?;
        let graph = closure(tx, &endpoints).await?;
        hold(tx, &graph).await?;
        let lines: Vec<i64> = graph.lines.iter().copied().collect();
        // every item the removal changes is the whole match walk (its settlements go, its residuals
        // are rebuilt), not only the two ends: the lock dates of all their entries, read after the
        // headers are held
        crate::moves::refuse_locked_items(tx, &env, &lines, "unmatch the items of").await?;
        unsettle(tx, &lines).await?;
        let companies: Vec<_> = companies.into_iter().collect();
        let days = erp_addon_base::company_calendar::business_days(tx, &env, &companies).await?;
        let (company_ids, removal_dates): (Vec<_>, Vec<_>) =
            days.into_iter().map(|(id, day)| (id, day.date)).unzip();
        let count = tx.execute(
            "WITH gone AS (DELETE FROM account_partial_reconcile WHERE id = ANY($1::int8[]) \
             RETURNING id, debit_move_id, credit_move_id, company_id, debit_currency_id, \
                       credit_currency_id, amount, max_date) \
             INSERT INTO account_reconcile_removal (original_partial_id, company_id, account_id, \
                 line_id, partner_id, currency_id, matched_delta, matched_on, removed_on, \
                 create_uid, create_date, write_uid, write_date) \
             SELECT p.id, l.company_id, l.account_id, l.id, l.partner_id, co.currency_id, \
                    side.delta, p.max_date, c.removed_on, $4, transaction_timestamp() AT TIME ZONE 'UTC', \
                    $4, transaction_timestamp() AT TIME ZONE 'UTC' \
             FROM gone p CROSS JOIN LATERAL (VALUES \
                 (p.debit_move_id, p.debit_currency_id, -p.amount), \
                 (p.credit_move_id, p.credit_currency_id, p.amount)) AS side(line_id, currency_id, delta) \
             JOIN account_move_line l ON l.id = side.line_id \
             JOIN unnest($2::int8[], $3::date[]) c(company_id, removed_on) ON c.company_id = l.company_id \
             JOIN res_company co ON co.id = l.company_id \
             WHERE l.currency_id = side.currency_id \
               AND p.max_date IS NOT NULL",
            &[&ids, &company_ids, &removal_dates, &env.uid()],
        ).await?;
        if count != 2 * ids.len() as u64 {
            return Err(Error::validation(
                "Cannot remove a match without dated company-currency facts for both journal items.",
            ));
        }
        rebuild(tx, &lines).await?;
        let graph = closure(tx, &lines).await?;
        settle(tx, &graph, false).await?;
        refresh_moves_of(tx, &lines).await?;
        invalidate(tx).await
    })
    .await
}

/// Deletes the settlements held by `lines` and clears their links on items and matches.
async fn unsettle(tx: &mut Transaction, lines: &[i64]) -> Result<()> {
    tx.execute(
        "WITH gone AS (SELECT DISTINCT full_reconcile_id AS id FROM account_move_line \
                       WHERE id = ANY($1::int8[]) AND full_reconcile_id IS NOT NULL), \
         l AS (UPDATE account_move_line SET full_reconcile_id = NULL \
               WHERE full_reconcile_id IN (SELECT id FROM gone) RETURNING 1), \
         p AS (UPDATE account_partial_reconcile SET full_reconcile_id = NULL \
               WHERE full_reconcile_id IN (SELECT id FROM gone) RETURNING 1) \
         DELETE FROM account_full_reconcile WHERE id IN (SELECT id FROM gone)",
        &[&lines],
    )
    .await?;
    Ok(())
}

/// Rebuilds the open-item rows of `lines` from their amounts and the matches that remain (
/// D4: a removal reconstructs, never applies a second delta); an item without a match keeps no row.
async fn rebuild(tx: &mut Transaction, lines: &[i64]) -> Result<()> {
    let uid = tx.env().uid();
    tx.execute(
        "DELETE FROM account_open_item o WHERE o.line_id = ANY($1::int8[]) AND NOT EXISTS \
         (SELECT 1 FROM account_partial_reconcile p \
          WHERE p.debit_move_id = o.line_id OR p.credit_move_id = o.line_id)",
        &[&lines],
    )
    .await?;
    tx.execute(
        "INSERT INTO account_open_item (line_id, company_id, account_id, partner_id, currency_id, \
         balance, amount_currency, amount_residual, amount_residual_currency, component_id, \
         create_uid, create_date, write_uid, write_date) \
         SELECT l.id, l.company_id, l.account_id, l.partner_id, l.currency_id, \
                COALESCE(l.balance, 0), COALESCE(l.amount_currency, 0), \
                COALESCE(l.balance, 0) - COALESCE(dr.amount, 0) + COALESCE(cr.amount, 0), \
                COALESCE(l.amount_currency, 0) - COALESCE(dr.amount_currency, 0) \
                    + COALESCE(cr.amount_currency, 0), \
                0, $2, now() AT TIME ZONE 'UTC', $2, now() AT TIME ZONE 'UTC' \
         FROM account_move_line l \
         LEFT JOIN (SELECT debit_move_id AS id, SUM(amount) AS amount, \
                           SUM(debit_amount_currency) AS amount_currency \
                    FROM account_partial_reconcile WHERE debit_move_id = ANY($1::int8[]) \
                    GROUP BY debit_move_id) dr ON dr.id = l.id \
         LEFT JOIN (SELECT credit_move_id AS id, SUM(amount) AS amount, \
                           SUM(credit_amount_currency) AS amount_currency \
                    FROM account_partial_reconcile WHERE credit_move_id = ANY($1::int8[]) \
                    GROUP BY credit_move_id) cr ON cr.id = l.id \
         WHERE l.id = ANY($1::int8[]) AND (dr.id IS NOT NULL OR cr.id IS NOT NULL) ORDER BY l.id \
         ON CONFLICT (line_id) DO UPDATE SET amount_residual = EXCLUDED.amount_residual, \
             amount_residual_currency = EXCLUDED.amount_residual_currency, \
             write_uid = EXCLUDED.write_uid, write_date = EXCLUDED.write_date",
        &[&lines, &uid],
    )
    .await?;
    Ok(())
}

async fn refresh_moves_of(tx: &mut Transaction, lines: &[i64]) -> Result<()> {
    let rows = tx
        .query(
            "SELECT DISTINCT move_id::int8 FROM account_move_line WHERE id = ANY($1::int8[]) \
             ORDER BY 1",
            &[&lines],
        )
        .await?;
    let moves: Vec<i64> = rows
        .iter()
        .map(|row| row.try_get::<_, i64>(0).map_err(db))
        .collect::<Result<_>>()?;
    refresh_moves(tx, &moves).await
}

/// The one transition function of an invoice's amount due and payment status (A4
/// W9), run in the reconciliation unit on the posted invoices among `moves` (a blocked invoice
/// keeps its status). The amount due is what remains on its receivable or payable items, in the
/// document currency; the status comes from [`plan::payment_state`].
pub(crate) async fn refresh_moves(tx: &mut Transaction, moves: &[i64]) -> Result<()> {
    refresh_invoices(tx, moves).await?;
    // the payments read the invoices' new status
    crate::payment::refresh_payments(tx, moves).await
}

async fn refresh_invoices(tx: &mut Transaction, moves: &[i64]) -> Result<()> {
    let types: Vec<String> = INVOICE_TYPES.iter().map(|t| (*t).to_owned()).collect();
    let rows = tx
        .query(
            "SELECT m.id::int8, m.move_type::text, m.payment_state::text, \
             COALESCE(SUM(CASE WHEN o.line_id IS NOT NULL THEN o.amount_residual_currency \
                               WHEN l.full_reconcile_id IS NOT NULL THEN 0 \
                               ELSE l.amount_currency END), 0) \
             FROM account_move m \
             LEFT JOIN account_move_line l ON l.move_id = m.id AND l.display_type = 'payment_term' \
             LEFT JOIN account_open_item o ON o.line_id = l.id \
             WHERE m.id = ANY($1::int8[]) AND m.state = 'posted' AND m.move_type = ANY($2::text[]) \
             AND m.payment_state IS DISTINCT FROM 'blocked' \
             GROUP BY m.id, m.move_type, m.payment_state ORDER BY m.id",
            &[&moves, &types],
        )
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut invoices = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(db)?;
        let kind: String = row.try_get(1).map_err(db)?;
        let old_state: Option<String> = row.try_get(2).map_err(db)?;
        let residual: Decimal = row.try_get(3).map_err(db)?;
        invoices.push((id, kind, old_state, residual.abs()));
    }
    let ids: Vec<i64> = invoices.iter().map(|(id, ..)| *id).collect();
    let mut settlements: BTreeMap<i64, Settlement> = BTreeMap::new();
    for (id, kind, paid) in counterparts(tx, &ids).await? {
        let entry = settlements.entry(id).or_default();
        entry.counterpart_types.insert(kind);
        entry.has_payment |= paid;
    }
    let none = Settlement::default();
    let mut residuals = Vec::with_capacity(invoices.len());
    let mut states = Vec::with_capacity(invoices.len());
    for (id, kind, _, residual) in &invoices {
        let matches = settlements.get(id).unwrap_or(&none);
        states.push(plan::payment_state(kind, residual.is_zero(), matches).to_owned());
        residuals.push(*residual);
    }
    tx.execute(
        "UPDATE account_move m SET amount_residual = v.residual, payment_state = v.state \
         FROM unnest($1::int8[], $2::numeric[], $3::text[]) AS v(id, residual, state) \
         WHERE m.id = v.id",
        &[&ids, &residuals, &states],
    )
    .await?;
    // the payment status in the invoice's history (a no-op without a tracking addon)
    let cell = |state: Option<&str>| state.map_or(Cell::Null, |s| Cell::Str(s.into()));
    let records = tx
        .env()
        .records("account.move")?
        .browse(ids.iter().copied())?;
    tx.track_external(
        &records,
        "payment_state",
        invoices
            .iter()
            .zip(&states)
            .map(|((id, _, old, _), new)| (*id, cell(old.as_deref()), cell(Some(new)))),
    )
}

/// For each invoice of `ids`: the kinds of the other entries matched with its receivable or payable
/// items, and whether such an entry belongs to a payment (reference).
async fn counterparts(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<(i64, String, bool)>> {
    let rows = tx
        .query(
            &format!(
                "SELECT src.move_id::int8, cm.move_type::text, bool_or({}) \
             FROM account_partial_reconcile p \
             JOIN account_move_line src ON src.id IN (p.debit_move_id, p.credit_move_id) \
             JOIN account_move_line cl ON cl.id = CASE WHEN src.id = p.debit_move_id \
                  THEN p.credit_move_id ELSE p.debit_move_id END \
             JOIN account_move cm ON cm.id = cl.move_id \
             JOIN account_account a ON a.id = src.account_id \
             WHERE src.move_id = ANY($1::int8[]) AND cl.move_id <> src.move_id \
             AND a.account_type IN ('asset_receivable', 'liability_payable') \
             GROUP BY src.move_id, cm.move_type",
                PAYMENT_ENTRY_SQL
            ),
            &[&ids],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get(0).map_err(db)?,
                row.try_get(1).map_err(db)?,
                row.try_get(2).map_err(db)?,
            ))
        })
        .collect()
}

/// Whether the counterpart entry `cm` belongs to a payment (bank statement lines come later).
const PAYMENT_ENTRY_SQL: &str = "cm.origin_payment_id IS NOT NULL";

async fn invalidate(tx: &mut Transaction) -> Result<()> {
    tx.invalidate_model(
        "account.move.line",
        Some(&[
            "full_reconcile_id",
            "matched_debit_ids",
            "matched_credit_ids",
            "amount_residual",
            "amount_residual_currency",
            "reconciled",
            "matching_number",
        ]),
    )
    .await?;
    tx.invalidate_model(
        "account.move",
        Some(&["amount_residual", "payment_state", "reset_blocked"]),
    )
    .await?;
    for model in [
        "account.partial.reconcile",
        "account.full.reconcile",
        "account.open.item",
    ] {
        tx.invalidate_model(model, None).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/apply.rs"]
mod tests;

#[cfg(test)]
mod fx_tests {
    use super::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal literal")
    }

    #[test]
    fn foreign_currency_settlement_and_fx_difference_are_exact() {
        assert_eq!(
            settled_currency_amount(dec("50.00"), dec("100.00"), dec("100.00"), true, 2),
            dec("50.00")
        );
        assert_eq!(
            settled_currency_amount(dec("55.00"), dec("110.00"), dec("100.00"), false, 2),
            dec("50.00")
        );
        assert_eq!(
            settled_currency_amount(dec("110.00"), dec("110.00"), dec("100.00"), false, 2),
            dec("100.00")
        );
        assert_eq!(
            compute_fx_difference(dec("110.00"), dec("120.00")),
            dec("-10.00")
        );
    }
}
