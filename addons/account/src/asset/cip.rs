// SPDX-License-Identifier: MIT OR Apache-2.0
//! Construction in Progress (CIP / Asset Under Construction) cost accumulation ledger
//! (`account.asset.cip.line`) hooked automatically into journal entry posting and reset.

use erp_sdk::prelude::*;

use super::db;

/// `account.asset.cip.line`: one posted cost item accumulated onto a CIP asset from a vendor
/// bill or general ledger journal entry.
#[model(
    name = "account.asset.cip.line",
    description = "CIP Cost Accumulation Line",
    order = "date, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_unique_move_line",
        fields = ["move_line_id"],
        message = "A journal item can only generate one CIP accumulation line."
    )]
)]
pub struct AccountAssetCipLine {
    /// Target CIP asset.
    #[field(
        many2one,
        comodel = "account.asset",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "CIP Asset"
    )]
    pub asset_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "asset_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Company currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Originating posted journal item.
    #[field(
        many2one,
        comodel = "account.move.line",
        ondelete = "cascade",
        index = "btree_not_null",
        string = "Journal Item"
    )]
    pub move_line_id: Option<RecId>,
    /// Posting date.
    #[field(date, required = true, index = true, string = "Date")]
    pub date: Option<Date>,
    /// Vendor or partner, if any.
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        ondelete = "set null",
        string = "Partner"
    )]
    pub partner_id: Option<RecId>,
    /// CIP GL Account debited/credited.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Account"
    )]
    pub account_id: Option<RecId>,
    /// Line description.
    #[field(char, required = true, string = "Description")]
    pub name: Option<Arc<str>>,
    /// Signed accumulated CIP cost in company currency (debit positive, credit/refund negative).
    #[field(
        monetary,
        currency_field = "currency_id",
        string = "Accumulated Amount"
    )]
    pub amount: Decimal,
    /// Portion of this line already settled / capitalized out of CIP.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        default = 0,
        string = "Settled Amount"
    )]
    pub settled_amount: Decimal,
    /// Remaining unsettled balance (`amount - settled_amount`).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_unsettled",
        string = "Unsettled Amount"
    )]
    pub unsettled_amount: Decimal,
}

#[methods(AccountAssetCipLine, addon = "account")]
impl AccountAssetCipLine {
    #[compute(depends = ["amount", "settled_amount"])]
    fn _compute_unsettled(batch: &mut Batch<'_, AccountAssetCipLine>) -> Result<()> {
        for i in batch.indices() {
            let amt = batch.get(i, AccountAssetCipLine::AMOUNT)?;
            let settled = batch.get(i, AccountAssetCipLine::SETTLED_AMOUNT)?;
            batch.set(i, AccountAssetCipLine::UNSETTLED_AMOUNT, amt - settled)?;
        }
        Ok(())
    }
}

/// Validates and records CIP cost accumulation rows for any posted journal items in `move_ids`
/// tagged with a Construction in Progress (`asset_type = 'cip'`) asset.
pub async fn record_cip_lines_for_moves(
    tx: &mut Transaction,
    env: &Env,
    move_ids: &[i64],
) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT l.id::int8, l.asset_id::int8, l.date, l.partner_id::int8, \
                    l.account_id::int8, COALESCE(l.name, m.name, 'CIP Cost')::text, \
                    COALESCE(l.balance, 0), COALESCE(a.asset_type, 'normal')::text, \
                    COALESCE(a.state, 'draft')::text, a.name::text, \
                    l.company_id::int8, a.company_id::int8, a.account_asset_id::int8 \
             FROM account_move_line l \
             JOIN account_move m ON m.id = l.move_id \
             JOIN account_asset a ON a.id = l.asset_id \
             WHERE l.move_id = ANY($1::int8[]) \
               AND l.asset_id IS NOT NULL \
               AND COALESCE(l.balance, 0) <> 0 \
             ORDER BY l.id",
            &[&move_ids],
        )
        .await?;
    if rows.is_empty() {
        return Ok(());
    }

    let mut vals_list = Vec::new();
    let mut draft_cip_ids = Vec::new();
    let mut touched_assets = Vec::new();

    for row in rows {
        let move_line_id: i64 = row.try_get(0).map_err(db)?;
        let asset_id: i64 = row.try_get(1).map_err(db)?;
        let date: Date = row.try_get(2).map_err(db)?;
        let partner_id: Option<i64> = row.try_get(3).map_err(db)?;
        let account_id: i64 = row.try_get(4).map_err(db)?;
        let label: String = row.try_get(5).map_err(db)?;
        let balance: Decimal = row.try_get(6).map_err(db)?;
        let asset_type: String = row.try_get(7).map_err(db)?;
        let state: String = row.try_get(8).map_err(db)?;
        let asset_name: String = row.try_get(9).map_err(db)?;
        let line_company_id: i64 = row.try_get(10).map_err(db)?;
        let asset_company_id: i64 = row.try_get(11).map_err(db)?;
        let asset_gl_account_id: i64 = row.try_get(12).map_err(db)?;

        if line_company_id != asset_company_id {
            return Err(Error::validation(format!(
                "Journal item '{label}' cannot reference CIP asset '{asset_name}' from another company."
            )));
        }
        if asset_type != "cip" {
            continue;
        }
        if account_id != asset_gl_account_id {
            return Err(Error::validation(format!(
                "Journal item '{label}' tagged with CIP asset '{asset_name}' must post to the asset's CIP GL account."
            )));
        }
        if !matches!(state.as_str(), "draft" | "cip") {
            return Err(Error::user(format!(
                "CIP asset '{asset_name}' is already {state} and cannot accumulate additional costs."
            )));
        }
        if state == "draft" && !draft_cip_ids.contains(&asset_id) {
            draft_cip_ids.push(asset_id);
        }
        if !touched_assets.contains(&asset_id) {
            touched_assets.push(asset_id);
        }

        let mut v = RawVals::new();
        v.insert("asset_id".into(), Cell::Id(asset_id));
        v.insert("move_line_id".into(), Cell::Id(move_line_id));
        v.insert("date".into(), Cell::Date(date));
        v.insert("account_id".into(), Cell::Id(account_id));
        v.insert("name".into(), Cell::Str(label.into()));
        v.insert("amount".into(), Cell::Decimal(balance));
        if let Some(pid) = partner_id {
            v.insert("partner_id".into(), Cell::Id(pid));
        }
        vals_list.push(v);
    }

    if vals_list.is_empty() {
        return Ok(());
    }
    let clean_env = crate::moves::post::strip_default_context(env);
    clean_env
        .records("account.asset.cip.line")?
        .create(tx, vals_list)
        .await?;

    if !draft_cip_ids.is_empty() {
        tx.execute(
            "UPDATE account_asset SET state = 'cip', write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = ANY($1::int8[]) AND state = 'draft'",
            &[&draft_cip_ids],
        )
        .await?;
    }
    tx.invalidate_model(
        "account.asset",
        Some(&[
            "state",
            "cip_line_ids",
            "cip_accumulated_amount",
            "cip_settled_amount",
            "cip_unsettled_amount",
            "value_residual",
        ]),
    )
    .await
}

/// Removes CIP accumulation lines when their originating journal entries are reset to draft;
/// refuses if any linked CIP cost line has already been settled or its CIP asset is already settled/closed.
pub async fn remove_cip_lines_for_moves(tx: &mut Transaction, move_ids: &[i64]) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    let settled_count: i64 = tx
        .query(
            "SELECT count(*)::int8 \
             FROM account_asset_cip_line c \
             JOIN account_move_line l ON l.id = c.move_line_id \
             JOIN account_asset a ON a.id = c.asset_id \
             WHERE l.move_id = ANY($1::int8[]) \
               AND (COALESCE(c.settled_amount, 0) <> 0 \
                    OR a.state IN ('settled', 'close', 'disposed'))",
            &[&move_ids],
        )
        .await?[0]
        .try_get(0)
        .map_err(db)?;
    if settled_count > 0 {
        return Err(Error::user(
            "Cannot reset a journal entry whose CIP asset or cost lines have already been settled. Reverse the CIP settlement first.",
        ));
    }
    tx.execute(
        "DELETE FROM account_asset_cip_line c \
         USING account_move_line l \
         WHERE c.move_line_id = l.id AND l.move_id = ANY($1::int8[])",
        &[&move_ids],
    )
    .await?;
    tx.invalidate_model(
        "account.asset",
        Some(&[
            "cip_line_ids",
            "cip_accumulated_amount",
            "cip_settled_amount",
            "cip_unsettled_amount",
            "value_residual",
        ]),
    )
    .await
}
