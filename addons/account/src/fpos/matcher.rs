// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{id, text};
use erp_sdk::{Operation, prelude::*};

pub(super) async fn resolve(
    model: &Records,
    tx: &mut Transaction,
    partner_id: i64,
    delivery_id: Option<i64>,
) -> Result<Option<i64>> {
    let company_id = model
        .env()
        .company_id()?
        .ok_or_else(|| Error::validation("Select a company before resolving a fiscal position."))?;
    let partner = model.env().records("res.partner")?.browse([partner_id])?;
    partner.check_access(tx, Operation::Read).await?;
    let company = model.env().records("res.company")?.browse([company_id])?;
    let company_partner = company.related(tx, "partner_id").await?;
    let mut address = partner.clone();
    if let Some(delivery) = delivery_id {
        address = model.env().records("res.partner")?.browse([delivery])?;
    }
    let company_vat = text(company.cell(tx, "vat").await?)?;
    let partner_vat = text(partner.cell(tx, "vat").await?)?;
    if company_vat.len() >= 2 && partner_vat.len() >= 2 {
        // Prefix extraction is Unicode-safe; country membership comes from installed data.
        let a: String = company_vat.chars().take(2).collect();
        let b: String = partner_vat.chars().take(2).collect();
        if a == b
            && company_partner.cell(tx, "country_id").await?
                == partner.cell(tx, "country_id").await?
        {
            let eu=tx.query("SELECT EXISTS (SELECT 1 FROM ir_model_data d JOIN res_country_res_country_group_rel r ON r.res_country_group_id=d.res_id JOIN res_country c ON c.id=r.res_country_id WHERE d.module='base' AND d.name='europe' AND d.model='res.country.group' AND c.code=$1)",&[&a]).await?;
            if eu[0].get::<_, bool>(0) {
                address = partner.clone();
            }
        }
    }
    address.check_access(tx, Operation::Read).await?;
    for p in [&address, &partner] {
        if let Some(position) = id(p.cell(tx, "property_account_position_id").await?)? {
            model
                .browse([position])?
                .check_access(tx, Operation::Read)
                .await?;
            return Ok(Some(position));
        }
    }
    if id(partner.cell(tx, "country_id").await?)?.is_none() {
        return Ok(None);
    }
    let candidates = model
        .search(tx, &Domain::field("auto_apply")?.eq(true))
        .await?;
    // Candidate ids were admitted by the caller's ORM search. Hierarchy depth is an
    // integrity fact: reading an ancestor company through ORM would deny a branch user
    // even though its fiscal positions are explicitly readable.
    tx.flush().await?;
    let ids = candidates.ids();
    let ordered=tx.query(
        "SELECT f.id FROM account_fiscal_position f JOIN res_company owner ON owner.id=f.company_id \
         JOIN res_company selected ON selected.id=$2 \
         WHERE f.id=ANY($1::int8[]) AND selected.parent_path LIKE owner.parent_path || '%' \
         ORDER BY length(owner.parent_path)-length(replace(owner.parent_path,'/','')) DESC,f.sequence,f.id",
        &[&ids,&company_id]).await?;
    for row in ordered {
        let candidate: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("Fiscal position order: {e}")))?;
        let rec = model.browse([candidate])?;
        if rec
            .call::<_, bool>(tx, "_fpos_matches", (address.ids()[0],))
            .await?
        {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}
pub(super) async fn matches(
    position: &Records,
    tx: &mut Transaction,
    partner_id: i64,
) -> Result<bool> {
    position.check_access(tx, Operation::Read).await?;
    let partner = position
        .env()
        .records("res.partner")?
        .browse([partner_id])?;
    if position.cell(tx, "vat_required").await? == Cell::Bool(true)
        && !partner
            .call::<_, bool>(tx, "_get_vat_required_valid", ())
            .await?
    {
        return Ok(false);
    }
    let lower = text(position.cell(tx, "zip_from").await?)?;
    let upper = text(position.cell(tx, "zip_to").await?)?;
    if !lower.is_empty() && !upper.is_empty() {
        let zip = text(partner.cell(tx, "zip").await?)?;
        if zip.is_empty() || zip < lower || zip > upper {
            return Ok(false);
        }
    }
    let state = id(partner.cell(tx, "state_id").await?)?;
    let states = position.related(tx, "state_ids").await?;
    if !states.is_empty() && !state.is_some_and(|id| states.ids().contains(&id)) {
        return Ok(false);
    }
    let country = id(partner.cell(tx, "country_id").await?)?;
    if let Some(expected) = id(position.cell(tx, "country_id").await?)?
        && country != Some(expected)
    {
        return Ok(false);
    }
    let group = position.related(tx, "country_group_id").await?;
    if !group.is_empty() {
        if !country.is_some_and(|id| id > 0) {
            return Ok(false);
        }
        if !group
            .related(tx, "country_ids")
            .await?
            .ids()
            .iter()
            .any(|id| Some(*id) == country)
        {
            return Ok(false);
        }
        if let Some(state) = state
            && group
                .related(tx, "exclude_state_ids")
                .await?
                .ids()
                .contains(&state)
        {
            return Ok(false);
        }
    }
    Ok(true)
}
