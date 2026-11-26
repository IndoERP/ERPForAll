// SPDX-License-Identifier: MIT OR Apache-2.0
//! Controlling & Cost Center Accounting Hooks: GL links on analytic lines, product-based
//! default cost center derivation, and posting validation (mandatory policy, validity windows,
//! primary cost locks, direct revenue locks, and commitment locks).

use erp_addon_analytic::{
    AccountAnalyticDistributionModel, AccountAnalyticLine, is_center_valid_on,
};
use erp_sdk::prelude::*;

fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Controlling query failed: {err}"))
}

/// Extends `account.analytic.line` with links to GL journal items and GL accounts.
#[extend(AccountAnalyticLine, addon = "account")]
pub struct AccountAnalyticLineAccounting {
    /// Originating posted journal item, when generated from a GL entry.
    #[field(
        many2one,
        comodel = "account.move.line",
        ondelete = "cascade",
        index = "btree_not_null",
        string = "Journal Item"
    )]
    pub move_line_id: Option<RecId>,
    /// General ledger account associated with this analytic line.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "set null",
        index = "btree_not_null",
        string = "Financial Account"
    )]
    pub general_account_id: Option<RecId>,
}

/// Extends `account.analytic.distribution.model` with product matching for default cost center
/// derivation.
#[extend(AccountAnalyticDistributionModel, addon = "account")]
pub struct AccountAnalyticDistributionProduct {
    /// Optional product filter for default cost center assignment.
    #[field(
        many2one,
        comodel = "product.product",
        ondelete = "cascade",
        string = "Product"
    )]
    pub product_id: Option<RecId>,
}

/// Applies default cost center assignment rules (`account.analytic.distribution.model`) to
/// accountable lines of `move_ids` that do not yet specify an analytic account.
///
/// Runs in a single set-based statement so batch posting stays O(1) in query count.
pub async fn apply_default_cost_centers(tx: &mut Transaction, move_ids: &[i64]) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    let updated = tx
        .execute(
            "WITH matched AS ( \
               SELECT DISTINCT ON (l.id) \
                 l.id AS line_id, \
                 m.analytic_account_id \
               FROM account_move_line l \
               JOIN account_account a ON a.id = l.account_id \
               JOIN res_company comp ON comp.id = l.company_id \
               LEFT JOIN account_account_code cm \
                 ON cm.account_id = l.account_id \
                AND cm.company_id = COALESCE(NULLIF(split_part(comp.parent_path, '/', 1), '')::int8, l.company_id) \
               JOIN account_analytic_distribution_model m \
                 ON COALESCE(m.active, true) = true \
                AND (m.company_id = l.company_id \
                     OR (comp.parent_path IS NOT NULL \
                         AND EXISTS (SELECT 1 FROM res_company mc \
                                     WHERE mc.id = m.company_id \
                                       AND mc.parent_path IS NOT NULL \
                                       AND starts_with(comp.parent_path, mc.parent_path)))) \
                AND (m.partner_id IS NULL OR m.partner_id = l.partner_id) \
                AND (m.product_id IS NULL OR m.product_id = l.product_id) \
                AND (m.account_prefix IS NULL OR m.account_prefix = '' \
                     OR starts_with(COALESCE(cm.code, ''), m.account_prefix)) \
               WHERE l.move_id = ANY($1::int8[]) \
                 AND l.analytic_account_id IS NULL \
                 AND l.account_id IS NOT NULL \
                 AND COALESCE(l.balance, 0) <> 0 \
                 AND COALESCE(a.cost_center_policy, 'optional') <> 'forbidden' \
               ORDER BY l.id, \
                 (CASE WHEN m.company_id = l.company_id THEN 1 ELSE 0 END) DESC, \
                 (CASE WHEN m.product_id IS NOT NULL THEN 4 ELSE 0 END \
                + CASE WHEN m.partner_id IS NOT NULL THEN 2 ELSE 0 END \
                + CASE WHEN m.account_prefix IS NOT NULL AND m.account_prefix <> '' THEN 1 ELSE 0 END) DESC, \
                 m.sequence ASC, m.id ASC \
             ) \
             UPDATE account_move_line l \
             SET analytic_account_id = matched.analytic_account_id \
             FROM matched \
             WHERE l.id = matched.line_id",
            &[&move_ids],
        )
        .await?;
    if updated > 0 {
        tx.invalidate_model("account.move.line", Some(&["analytic_account_id"]))
            .await?;
    }
    Ok(())
}

/// Validates cost center policy (`mandatory` / `forbidden`), validity windows (`valid_from..valid_to`),
/// and posting locks (`lock_primary_costs`, `lock_revenues`) for all lines of `move_dates`
/// (each `(move_id, resolved_accounting_date)`).
pub async fn validate_move_cost_centers(
    tx: &mut Transaction,
    move_dates: &[(i64, Date)],
) -> Result<()> {
    if move_dates.is_empty() {
        return Ok(());
    }
    let (move_ids, dates): (Vec<i64>, Vec<Date>) = move_dates.iter().copied().unzip();
    let rows = tx
        .query(
            "WITH md AS (SELECT * FROM unnest($1::int8[], $2::date[]) AS t(move_id, eff_date)) \
             SELECT l.id::int8, COALESCE(l.name, m.name, 'Journal Item')::text, md.eff_date, \
                    COALESCE(l.balance, 0), COALESCE(a.cost_center_policy, 'optional')::text, \
                    COALESCE(a.name->>'en_US', \
                             (SELECT v FROM jsonb_each_text(a.name) AS kv(k, v) LIMIT 1), \
                             'Account')::text, \
                    COALESCE(a.internal_group, '')::text, \
                    l.analytic_account_id::int8, \
                    cc.name::text, cc.code::text, cc.valid_from, cc.valid_to, \
                    COALESCE(cc.lock_primary_costs, false), \
                    COALESCE(cc.lock_revenues, false), \
                    COALESCE(cc.active, true) \
             FROM md \
             JOIN account_move m ON m.id = md.move_id \
             JOIN account_move_line l ON l.move_id = md.move_id \
             JOIN account_account a ON a.id = l.account_id \
             LEFT JOIN account_analytic_account cc ON cc.id = l.analytic_account_id \
             WHERE l.account_id IS NOT NULL \
               AND COALESCE(l.balance, 0) <> 0 \
             ORDER BY l.id",
            &[&move_ids, &dates],
        )
        .await?;

    let mut errors = Vec::new();
    for row in rows {
        let _line_id: i64 = row.try_get(0).map_err(db)?;
        let label: String = row.try_get(1).map_err(db)?;
        let entry_date: Date = row.try_get(2).map_err(db)?;
        let _balance: Decimal = row.try_get(3).map_err(db)?;
        let policy: String = row.try_get(4).map_err(db)?;
        let account_name: String = row.try_get(5).map_err(db)?;
        let internal_group: String = row.try_get(6).map_err(db)?;
        let cc_id: Option<i64> = row.try_get(7).map_err(db)?;
        let cc_name: Option<String> = row.try_get(8).map_err(db)?;
        let cc_code: Option<String> = row.try_get(9).map_err(db)?;
        let valid_from: Option<Date> = row.try_get(10).map_err(db)?;
        let valid_to: Option<Date> = row.try_get(11).map_err(db)?;
        let lock_primary: bool = row.try_get(12).map_err(db)?;
        let lock_revenues: bool = row.try_get(13).map_err(db)?;
        let cc_active: bool = row.try_get(14).map_err(db)?;

        match (policy.as_str(), cc_id) {
            ("optional", _) => {}
            ("mandatory", None) => {
                errors.push(format!(
                    "Account '{account_name}' on line '{label}' requires a Cost Center / Analytic Account."
                ));
                continue;
            }
            ("mandatory", Some(_)) => {}
            ("forbidden", Some(_)) => {
                errors.push(format!(
                    "Account '{account_name}' on line '{label}' does not allow a Cost Center / Analytic Account."
                ));
                continue;
            }
            ("forbidden", None) => {}
            (other, _) => {
                return Err(Error::validation(format!(
                    "Unknown cost_center_policy '{other}' on account '{account_name}'."
                )));
            }
        }

        if let Some(cid) = cc_id {
            let Some(name_str) = cc_name.as_deref() else {
                return Err(Error::missing(format!(
                    "Cost center {cid} referenced on line '{label}' does not exist."
                )));
            };
            let cc_label = match cc_code.as_deref() {
                Some(c) if !c.is_empty() => format!("[{c}] {name_str}"),
                _ => name_str.to_owned(),
            };
            if !cc_active {
                errors.push(format!(
                    "Cost Center {cc_label} is archived and cannot receive postings."
                ));
            }
            if !is_center_valid_on(valid_from, valid_to, entry_date) {
                errors.push(format!(
                    "Cost Center {cc_label} is not valid on {entry_date} (line '{label}')."
                ));
            }
            if lock_primary && internal_group == "expense" {
                errors.push(format!(
                    "Cost Center {cc_label} is locked for primary cost postings (line '{label}')."
                ));
            }
            if lock_revenues && internal_group == "income" {
                errors.push(format!(
                    "Cost Center {cc_label} is locked for direct revenue postings (line '{label}')."
                ));
            }
        }
    }

    if !errors.is_empty() {
        return Err(Error::user(errors.join("\n")));
    }
    Ok(())
}

/// Validates that a cost center is open for new purchase order commitments on `date`.
pub async fn validate_center_for_commitment(
    tx: &mut Transaction,
    center_id: i64,
    date: Date,
) -> Result<()> {
    let rows = tx
        .query(
            "SELECT name::text, code::text, valid_from, valid_to, \
                    COALESCE(lock_commitment, false), COALESCE(active, true) \
             FROM account_analytic_account WHERE id = $1",
            &[&center_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(Error::missing(format!(
            "Cost center {center_id} does not exist."
        )));
    };
    let name: String = row.try_get(0).map_err(db)?;
    let code: Option<String> = row.try_get(1).map_err(db)?;
    let valid_from: Option<Date> = row.try_get(2).map_err(db)?;
    let valid_to: Option<Date> = row.try_get(3).map_err(db)?;
    let lock_commitment: bool = row.try_get(4).map_err(db)?;
    let active: bool = row.try_get(5).map_err(db)?;
    let label = match code.as_deref() {
        Some(c) if !c.is_empty() => format!("[{c}] {name}"),
        _ => name,
    };
    if !active {
        return Err(Error::user(format!(
            "Cost Center {label} is archived and cannot accept new purchase commitments."
        )));
    }
    if lock_commitment {
        return Err(Error::user(format!(
            "Cost Center {label} is locked for commitments."
        )));
    }
    if !is_center_valid_on(valid_from, valid_to, date) {
        return Err(Error::user(format!(
            "Cost Center {label} is outside its validity period on {date}."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use erp_addon_analytic::split_proportional_exact;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn skf_headcount_allocation_is_zero_sum_and_exact() {
        let it_pool = Decimal::from_str("2500.00").unwrap();
        let weights = vec![
            (10, Decimal::from_str("12").unwrap()),
            (20, Decimal::from_str("5").unwrap()),
            (30, Decimal::from_str("3").unwrap()),
        ];
        let shares = split_proportional_exact(it_pool, &weights, 2).unwrap();
        assert_eq!(shares[0].1, Decimal::from_str("1500.00").unwrap());
        assert_eq!(shares[1].1, Decimal::from_str("625.00").unwrap());
        assert_eq!(shares[2].1, Decimal::from_str("375.00").unwrap());
        let net_sender_plus_receivers = it_pool - shares.iter().map(|(_, s)| *s).sum::<Decimal>();
        assert_eq!(net_sender_plus_receivers, Decimal::ZERO);
    }
}
