// SPDX-License-Identifier: MIT OR Apache-2.0
//! Mutation preparation and database-owned cross-row consistency.
use super::{id, text};
use erp_sdk::{ErrorKind, Operation, prelude::*};
use std::collections::BTreeSet;

fn normalize_vat(value: &str) -> Result<String> {
    let normalized: String = value
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '.' | '-'))
        .flat_map(char::to_uppercase)
        .collect();
    if normalized.is_empty() {
        return Err(Error::validation(
            "foreign_vat must contain a tax identifier after normalization.",
        ));
    }
    Ok(normalized)
}
pub(super) async fn prepare(
    model: &Records,
    tx: &mut Transaction,
    mut vals: RawVals,
    existing: Option<&Records>,
) -> Result<RawVals> {
    for name in ["company_id", "zip_from", "zip_to", "foreign_vat"] {
        if !vals.contains(name) {
            let cell = if let Some(rec) = existing {
                rec.cell(tx, name).await?
            } else if name == "company_id" {
                let defaults = model.default_get(tx, &["company_id"]).await?;
                match defaults.get(name) {
                    Some(Value::Int(id)) => Cell::Id(*id),
                    Some(Value::False) | None => Cell::Null,
                    other => {
                        return Err(Error::validation(format!(
                            "Invalid company_id default: {other:?}"
                        )));
                    }
                }
            } else {
                Cell::Null
            };
            vals.insert(name.into(), cell);
        }
    }
    let company = id(vals
        .get("company_id")
        .cloned()
        .ok_or_else(|| Error::internal("Missing prepared company_id."))?)
    .map_err(|_| Error::validation("company_id must be a positive record id."))?
    .ok_or_else(|| Error::validation("Select company_id before configuring a fiscal position."))?;
    // A caller may configure only its selected company hierarchy; SQL below is integrity-only.
    if !model.env().is_superuser() {
        let allowed: Vec<i64> = model.env().companies()?.iter().map(|id| id.get()).collect();
        let ok:bool=tx.query("SELECT EXISTS (SELECT 1 FROM res_company c JOIN res_company a ON a.parent_path LIKE c.parent_path || '%' WHERE c.id=$1 AND a.id=ANY($2::int8[]))",&[&company,&allowed]).await?[0].get(0);
        if !ok {
            return Err(Error::access(
                "The fiscal position company is outside the active company hierarchy.",
            ));
        }
    }
    let mut lower = text(
        vals.get("zip_from")
            .cloned()
            .ok_or_else(|| Error::internal("Missing prepared zip_from."))?,
    )
    .map_err(|_| Error::validation("zip_from must be text or empty."))?;
    let mut upper = text(
        vals.get("zip_to")
            .cloned()
            .ok_or_else(|| Error::internal("Missing prepared zip_to."))?,
    )
    .map_err(|_| Error::validation("zip_to must be text or empty."))?;
    let width = lower.len().max(upper.len());
    if !lower.is_empty() && !upper.is_empty() {
        if lower.bytes().all(|b| b.is_ascii_digit()) {
            lower = format!("{lower:0>width$}");
        }
        if upper.bytes().all(|b| b.is_ascii_digit()) {
            upper = format!("{upper:0>width$}");
        }
    }
    if lower.is_empty() != upper.is_empty() || lower > upper {
        return Err(Error::validation(
            "Set both zip_from and zip_to, with zip_to at least zip_from.",
        ));
    }
    vals.insert(
        "zip_from".into(),
        if lower.is_empty() {
            Cell::Null
        } else {
            Cell::Str(lower.into())
        },
    );
    vals.insert(
        "zip_to".into(),
        if upper.is_empty() {
            Cell::Null
        } else {
            Cell::Str(upper.into())
        },
    );
    match vals.get("foreign_vat") {
        Some(Cell::Str(value)) => {
            let normalized = normalize_vat(value)?;
            vals.insert("foreign_vat".into(), Cell::Str(normalized.into()));
        }
        Some(Cell::Null) | None => {}
        _ => return Err(Error::validation("foreign_vat must be text or empty.")),
    }
    Ok(vals)
}
pub(super) async fn fence(
    records: &Records,
    tx: &mut Transaction,
    proposed: &[RawVals],
) -> Result<()> {
    let mut companies = BTreeSet::new();
    for vals in proposed {
        if let Some(Cell::Id(id)) = vals.get("company_id") {
            companies.insert(*id);
        }
    }
    for rec in records.iter() {
        if let Some(id) = id(rec.cell(tx, "company_id").await?)? {
            companies.insert(id);
        }
    }
    tx.flush().await?;
    let companies: Vec<i64> = companies.into_iter().collect();
    let roots=tx.query("SELECT DISTINCT split_part(parent_path,'/',1)::int8 AS root FROM res_company WHERE id=ANY($1::int8[]) ORDER BY root",&[&companies]).await?;
    for row in roots {
        let root: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("Fiscal company root: {e}")))?;
        // PostgreSQL int4 arithmetic is checked: exhaustion errors, never wraps. An UPDATE
        // makes a stale REPEATABLE READ writer fail; a lock-only preflight would not.
        if tx
            .execute(
                "UPDATE res_company SET fpos_revision=fpos_revision+1 WHERE id=$1",
                &[&root],
            )
            .await?
            != 1
        {
            return Err(Error::db("Fiscal position company-root fence is missing."));
        }
    }
    Ok(())
}
pub(super) async fn validate(records: &Records, tx: &mut Transaction) -> Result<()> {
    tx.flush().await?;
    for rec in records.iter() {
        let vat = text(rec.cell(tx, "foreign_vat").await?)?;
        if vat.is_empty() {
            continue;
        }
        let country = id(rec.cell(tx, "country_id").await?)?
            .ok_or_else(|| Error::validation("Set country_id before assigning foreign_vat."))?;
        let company_id = id(rec.cell(tx, "company_id").await?)?
            .ok_or_else(|| Error::internal("Fiscal position company is missing."))?;
        // The writer has already been admitted to this position's company hierarchy.
        // Its jurisdiction is an integrity fact, not permission to open the ancestor
        // company record (which branch managers deliberately cannot read).
        let company = tx
            .query(
                "SELECT account_fiscal_country_id::int8 FROM res_company WHERE id=$1",
                &[&company_id],
            )
            .await?;
        let fiscal_country: Option<i64> = company
            .first()
            .ok_or_else(|| Error::db("Fiscal position company is missing."))?
            .try_get(0)
            .map_err(|e| Error::db(format!("Fiscal company jurisdiction: {e}")))?;
        if fiscal_country == Some(country) {
            let states = records
                .env()
                .records("res.country.state")?
                .search(tx, &Domain::field("country_id")?.eq(country))
                .await?;
            if !states.is_empty() && rec.related(tx, "state_ids").await?.is_empty() {
                return Err(Error::validation(
                    "Set state_ids when foreign_vat is in the company's fiscal country.",
                ));
            }
        }
        let group = rec.related(tx, "country_group_id").await?;
        if !group.is_empty()
            && !group
                .related(tx, "country_ids")
                .await?
                .ids()
                .contains(&country)
        {
            return Err(Error::validation(
                "country_id must belong to country_group_id when foreign_vat is set.",
            ));
        }
        let own = rec.ids()[0];
        let conflicts=tx.query("SELECT f.id FROM account_fiscal_position f JOIN res_company owner ON owner.id=f.company_id JOIN res_company written ON written.id=$1 WHERE written.parent_path LIKE owner.parent_path || '%' AND f.country_id=$2 AND f.active AND f.foreign_vat IS NOT NULL AND f.foreign_vat<>'' AND f.foreign_vat<>$3 AND f.id<>$4 ORDER BY f.id",&[&company_id,&country,&vat,&own]).await?;
        if let Some(row) = conflicts.first() {
            let conflict = records.browse([row.get::<_, i64>(0)])?;
            let readable = match conflict.check_access(tx, Operation::Read).await {
                Ok(()) => true,
                Err(e) if e.kind() == ErrorKind::Access => false,
                Err(e) => return Err(e),
            };
            let message = if readable {
                format!(
                    "Fiscal position {:?} already uses a different foreign_vat for this country.",
                    text(conflict.cell(tx, "name").await?)?
                )
            } else {
                "Another fiscal position in the company tree already uses a different foreign_vat for this country.".to_owned()
            };
            return Err(Error::validation(message));
        }
    }
    Ok(())
}
