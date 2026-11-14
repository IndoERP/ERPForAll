// SPDX-License-Identifier: MIT OR Apache-2.0
//! Detailed duplicate diagnostics before SQL mutation; the partial index remains authoritative.
use super::DUPLICATE_NAME;
use erp_sdk::prelude::*;
use erp_sdk::serde_json;
use std::collections::BTreeSet;

pub(crate) async fn check(
    model: &Records,
    tx: &mut Transaction,
    values: &[RawVals],
    writing: bool,
) -> Result<()> {
    // Settle earlier permitted writes before the conflict query. The proposed name is normalized
    // through the existing draft writer, so language seeding and inactive-English rules stay core-owned.
    tx.flush().await?;
    let names = [
        "name",
        "company_id",
        "type_tax_use",
        "tax_scope",
        "country_id",
        "active",
    ];
    let defaults: erp_sdk::ValueMap = model
        .call(
            tx,
            "default_get",
            (names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),),
        )
        .await?;
    let defaults = model.vals_from_values(&defaults)?;
    let mut candidates = Vec::with_capacity(values.len());
    for (index, vals) in values.iter().enumerate() {
        let existing = if writing {
            Some(model.browse([model.ids()[index]])?)
        } else {
            None
        };
        let mut cells = RawVals::new();
        for field in names {
            let cell = if let Some(value) = vals.get(field) {
                value.clone()
            } else if let Some(record) = &existing {
                record.cell(tx, field).await?
            } else if let Some(value) = defaults.get(field) {
                value.clone()
            } else {
                Cell::Null
            };
            cells.insert(field.into(), cell);
        }
        let origin = if writing {
            model.rec_ids().get(index).copied()
        } else {
            None
        };
        let draft = tx.new_record(model, RawVals::new(), origin, None).await?;
        if writing {
            let _name = draft.cell(tx, "name").await?;
        }
        if let Some(name) = vals.get("name") {
            draft
                .write_base(tx, RawVals::new().set_cell("name", name.clone()))
                .await?;
        }
        let source = draft.get_field_translations(tx, "name").await?;
        let name = source
            .get("en_US")
            .ok_or_else(|| Error::validation("A tax needs a non-empty source-language name."))?;
        let string = |field: &str| -> Result<String> {
            match cells.get(field) {
                Some(Cell::Str(s)) => Ok(s.to_string()),
                Some(Cell::Null) if field == "tax_scope" => Ok(String::new()),
                other => Err(Error::validation(format!("Invalid tax {field}: {other:?}"))),
            }
        };
        let id = |field: &str| -> Result<i64> {
            match cells.get(field) {
                Some(Cell::Id(id)) if *id > 0 => Ok(*id),
                other => Err(Error::validation(format!(
                    "A tax needs a valid {field}: {other:?}"
                ))),
            }
        };
        let active = match cells.get("active") {
            Some(Cell::Bool(b)) => *b,
            other => {
                return Err(Error::validation(format!(
                    "Invalid tax active flag: {other:?}"
                )));
            }
        };
        let use_ = string("type_tax_use")?;
        if active && use_ != "none" {
            // `ord` tells the rows of the batch apart: two creates both carry id 0
            let ord = candidates.len();
            candidates.push(serde_json::json!({"ord":ord,"id":if writing {model.ids()[index]} else {0},"company":id("company_id")?,"name":name,"use":use_,"scope":string("tax_scope")?,"country":id("country_id")?}));
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let candidates = serde_json::Value::Array(candidates);
    // The root is derived here from the written company (K1), not read from the record:
    // a company change has not recomputed the stored root yet.
    let rows = tx.query(
        "WITH proposed AS (SELECT p.*, COALESCE(NULLIF(split_part(c.parent_path,'/',1),''),c.id::text)::int8 AS root \
         FROM jsonb_to_recordset($1::jsonb) AS p(ord bigint, id bigint, company bigint, name text, \"use\" text, scope text, country bigint) JOIN res_company c ON c.id=p.company), conflicts AS ( \
         SELECT p.name,t.company_id FROM proposed p JOIN account_tax t ON t.root_company_id=p.root AND t.name->>'en_US'=p.name AND t.type_tax_use=p.\"use\" AND COALESCE(t.tax_scope,'')=p.scope AND t.country_id=p.country AND t.active AND t.type_tax_use<>'none' AND t.id<>p.id \
         UNION SELECT p.name,p.company FROM proposed p JOIN proposed q ON p.root=q.root AND p.name=q.name AND p.\"use\"=q.\"use\" AND p.scope=q.scope AND p.country=q.country AND p.ord<>q.ord) \
         SELECT DISTINCT x.name, c.name FROM conflicts x JOIN res_company c ON c.id=x.company_id ORDER BY x.name,c.name",
        &[&candidates]).await?;
    let mut conflicts = BTreeSet::new();
    for row in rows {
        let name: String = row
            .try_get(0)
            .map_err(|e| Error::db(format!("Tax name conflict: {e}")))?;
        let company: String = row
            .try_get(1)
            .map_err(|e| Error::db(format!("Tax company conflict: {e}")))?;
        conflicts.insert(format!("\"{name}\" is already used in {company}."));
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(Error::validation(format!(
            "{DUPLICATE_NAME}\n{}",
            conflicts.into_iter().collect::<Vec<_>>().join("\n")
        )))
    }
}
