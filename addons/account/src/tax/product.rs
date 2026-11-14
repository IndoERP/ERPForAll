// SPDX-License-Identifier: MIT OR Apache-2.0
//! Editable product tax choices for selected companies. Other-company inheritance
//! belongs to; no document consumer is implemented here.
use erp_addon_product::ProductTemplate;
use erp_sdk::prelude::*;
use erp_sdk::{Command, Operation, ValueMap};
use std::collections::BTreeSet;

const FIELDS: [(&str, &str); 2] = [
    ("taxes_id", "account_sale_tax_id"),
    ("supplier_taxes_id", "account_purchase_tax_id"),
];
#[extend(ProductTemplate, addon = "account")]
struct TaxProduct {
    /// Explicit sale tax choices; shared products may carry choices from several companies.
    /// replaces the field company check with caller admission plus selected/own ancestry.
    #[field(many2many, comodel = "account.tax", relation = "product_taxes_rel", column1 = "prod_id", column2 = "tax_id", check_company = false, domain = [("type_tax_use", "=", "sale")], string = "Taxes charged on sales")]
    pub taxes_id: Vec<RecId>,
    /// Explicit purchase tax choices, independently defaulted and admitted.
    #[field(many2many, comodel = "account.tax", relation = "product_supplier_taxes_rel", column1 = "prod_id", column2 = "tax_id", check_company = false, domain = [("type_tax_use", "=", "purchase")], string = "Taxes paid on purchases")]
    pub supplier_taxes_id: Vec<RecId>,
}
fn supplied(vals: &RawVals, field: &str) -> bool {
    vals.contains(field) || vals.commands(field).is_some()
}
#[methods(ProductTemplate, addon = "account")]
impl TaxProduct {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut values: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names
            .iter()
            .any(|n| FIELDS.iter().any(|(field, _)| n == field))
        {
            let mut defaults = selected_defaults(tx, this.env()).await?;
            if let Some(Value::Int(company)) = this.env().context().get("default_company_id") {
                let (allowed, _) = company_chain(tx, this.env(), &[company]).await?;
                for ids in &mut defaults {
                    let mut kept = Vec::new();
                    for tax in this
                        .env()
                        .records("account.tax")?
                        .browse(ids.iter().copied())?
                        .iter()
                    {
                        let Cell::Id(owner) = tax.cell(tx, "company_id").await? else {
                            return Err(Error::programming(
                                "A default product tax has no company.",
                            ));
                        };
                        if allowed.contains(&owner) {
                            kept.push(tax.ids()[0]);
                        }
                    }
                    *ids = kept;
                }
            }
            for ((field, _), ids) in FIELDS.iter().zip(defaults) {
                if names.iter().any(|n| n == field) && !values.contains_key(*field) {
                    let ids = ids
                        .into_iter()
                        .map(|id| {
                            RecId::from_real(id)
                                .ok_or_else(|| Error::programming("Invalid default tax identity."))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    values.insert(
                        (*field).to_owned(),
                        Value::list([Command::set(ids).to_value()]),
                    );
                }
            }
        }
        values.retain(|name, _| names.contains(name));
        Ok(values)
    }
    #[method(override)]
    async fn create(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        mut rows: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        super::admit(this.as_records(), tx, &rows, Operation::Create).await?;
        tx.savepoint(async |tx| {
            for vals in &mut rows {
                let missing: Vec<&str> = FIELDS
                    .iter()
                    .map(|(f, _)| *f)
                    .filter(|f| !supplied(vals, f))
                    .collect();
                if !missing.is_empty() {
                    let default_env = match vals.get("company_id") {
                        Some(Cell::Id(id)) => this
                            .env()
                            .with_context([("default_company_id", Value::Int(*id))]),
                        Some(Cell::Null) => this
                            .env()
                            .with_context([("default_company_id", Value::False)]),
                        // default context. Missing values retain the caller context; a wrong
                        // cell still reaches the normal create converter and is refused there.
                        _ => this.env().clone(),
                    };
                    let defaults = this
                        .as_records()
                        .with_env(default_env)
                        .default_get(tx, &missing)
                        .await?;
                    let defaults = this.as_records().vals_from_values(&defaults)?;
                    for field in missing {
                        if let Some(commands) = defaults.commands(field) {
                            vals.insert_commands(field.to_owned().into(), commands.to_vec());
                        } else if let Some(cell) = defaults.get(field) {
                            vals.insert(field.to_owned().into(), cell.clone());
                        }
                    }
                }
            }
            let created: Records = sup.call(tx, this.as_records(), (rows,)).await?;
            for record in created.iter() {
                for (field, _) in FIELDS {
                    validate_links(tx, &record, field, &BTreeSet::new()).await?;
                }
            }
            refuse_foreign_links(tx, &created.ids(), LINK_REFUSAL).await?;
            Ok(created)
        })
        .await
    }
    #[method(override)]
    async fn write(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        super::admit(
            this.as_records(),
            tx,
            std::slice::from_ref(&vals),
            Operation::Write,
        )
        .await?;
        tx.savepoint(async |tx| {
            let company_changes = vals.contains("company_id");
            let links_change = FIELDS.iter().any(|(field, _)| supplied(&vals, field));
            if matches!(vals.get("type"),Some(Cell::Str(kind)) if kind.as_ref()=="combo") {
                for (field, _) in FIELDS {
                    vals.remove(field);
                    vals.insert_commands(field.into(), vec![Command::clear()]);
                }
            }
            let mut previous = Vec::new();
            for record in this.as_records().iter() {
                for (field, _) in FIELDS {
                    if supplied(&vals, field) || company_changes {
                        let before = record
                            .related(tx, field)
                            .await?
                            .ids()
                            .into_iter()
                            .collect::<BTreeSet<_>>();
                        previous.push((record.clone(), field, before));
                    }
                }
            }
            // The ORM's many2many writer already preserves inaccessible existing links for
            // CLEAR/SET/UNLINK (commands.rs). Do not re-link them: LINK requires target read access.
            sup.call(tx, this.as_records(), (vals,)).await?;
            for (record, field, before) in previous {
                let previous = if company_changes {
                    BTreeSet::new()
                } else {
                    before
                };
                validate_links(tx, &record, field, &previous).await?;
            }
            // `validate_links` reads through `related`, which hides archived taxes: the stored
            // links are checked whenever they or the company may have changed.
            if company_changes {
                refuse_foreign_links(tx, &this.ids(), MOVE_REFUSAL).await?;
            } else if links_change {
                refuse_foreign_links(tx, &this.ids(), LINK_REFUSAL).await?;
            }
            Ok(())
        })
        .await
    }
}

fn parse_company_path(cell: Cell, owner: i64) -> Result<Vec<i64>> {
    let Cell::Str(path) = cell else {
        return Err(Error::programming(
            "Company ancestry is missing for product tax selection.",
        ));
    };
    let mut ids = Vec::new();
    for part in path.trim_end_matches('/').split('/') {
        let id = part.parse::<i64>().map_err(|_| {
            Error::programming("Company ancestry is invalid for product tax selection.")
        })?;
        if id <= 0 {
            return Err(Error::programming(
                "Company ancestry is invalid for product tax selection.",
            ));
        }
        ids.push(id);
    }
    if ids.last() != Some(&owner) {
        return Err(Error::programming(
            "Company ancestry does not end at its company.",
        ));
    }
    Ok(ids)
}
async fn company_chain(
    tx: &mut Transaction,
    env: &Env,
    ids: &[i64],
) -> Result<(BTreeSet<i64>, BTreeSet<i64>)> {
    let companies = env.records("res.company")?;
    let mut chain = BTreeSet::new();
    let mut roots = BTreeSet::new();
    for id in ids {
        let company = companies.browse([*id])?;
        let path = if company.has_access(tx, Operation::Read).await? {
            parse_company_path(company.cell(tx, "parent_path").await?, *id)?
        } else {
            // A branch editor can edit an ancestor-owned product without permission to open
            // that company's row. Its ancestry is already in the selected branch's parent_path.
            // Derive only the prefix already disclosed by an authorized selected-row read.
            let selected: Vec<i64> = tx.companies(env).await?.iter().map(|c| c.get()).collect();
            let mut prefix = None;
            for selected in companies.browse(selected)?.iter() {
                let path =
                    parse_company_path(selected.cell(tx, "parent_path").await?, selected.ids()[0])?;
                if let Some(index) = path.iter().position(|ancestor| ancestor == id) {
                    prefix = Some(path[..=index].to_vec());
                    break;
                }
            }
            match prefix {
                Some(path) => path,
                None => {
                    company.check_access(tx, Operation::Read).await?;
                    return Err(Error::programming(
                        "No authorized company ancestry was found.",
                    ));
                }
            }
        };
        let root = *path.first().ok_or_else(|| {
            Error::programming("Company ancestry is empty for product tax selection.")
        })?;
        roots.insert(root);
        chain.extend(path);
    }
    Ok((chain, roots))
}
async fn validate_links(
    tx: &mut Transaction,
    product: &Records,
    field: &str,
    before: &BTreeSet<i64>,
) -> Result<()> {
    let env = product.env();
    let selected: Vec<i64> = tx.companies(env).await?.iter().map(|c| c.get()).collect();
    let own = product.cell(tx, "company_id").await?;
    let companies = match own {
        Cell::Id(id) => vec![id],
        Cell::Null => selected,
        other => {
            return Err(Error::programming(format!(
                "Invalid product company: {other:?}"
            )));
        }
    };
    let (chain, _) = company_chain(tx, env, &companies).await?;
    let links = product.related(tx, field).await?;
    for tax in links.iter().filter(|tax| !before.contains(&tax.ids()[0])) {
        tax.check_access(tx, Operation::Read).await?;
        let Cell::Id(company) = tax.cell(tx, "company_id").await? else {
            return Err(Error::programming("A product tax has no company."));
        };
        if !chain.contains(&company) {
            // The caller passed the read check above, so the tax's name may be shown.
            let Cell::Str(name) = tax.cell(tx, "display_name").await? else {
                return Err(Error::programming("A product tax has no display name."));
            };
            let label = product
                .spec()
                .field(field)
                .map(|f| f.string.clone())
                .ok_or_else(|| Error::programming(format!("Product has no field '{field}'.")))?;
            return Err(Error::validation(format!(
                "The tax \"{name}\" cannot be chosen in {label}: it belongs to a company outside this product's companies and their parent companies."
            )));
        }
    }
    Ok(())
}

const MOVE_REFUSAL: &str = "The product cannot move to this company: it still has taxes of other companies. Someone who can see those taxes must remove them first.";
const LINK_REFUSAL: &str = "The product cannot have these taxes: some belong to a company outside this product's company and its parent companies.";

/// P2 for every stored link, also those `related` leaves out (unreadable or archived): a
/// company product keeps only taxes of that company or its parent companies. An integrity
/// check, not a tax-data read: only existence is inspected, and the refusal names no tax and
/// no company (the pattern of the account delete refusal). A company without ancestry is
/// refused too, never passed.
async fn refuse_foreign_links(tx: &mut Transaction, ids: &[i64], refusal: &str) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    // the new company must reach the table before it is compared
    tx.flush().await?;
    let foreign = tx
        .query(
            "SELECT 1 FROM (SELECT prod_id, tax_id FROM product_taxes_rel \
                            UNION ALL SELECT prod_id, tax_id FROM product_supplier_taxes_rel) r \
             JOIN account_tax t ON t.id = r.tax_id \
             JOIN product_template p ON p.id = r.prod_id \
             JOIN res_company c ON c.id = p.company_id \
             WHERE r.prod_id = ANY($1::int8[]) \
               AND (c.parent_path IS NULL \
                    OR t.company_id <> ALL (string_to_array(rtrim(c.parent_path, '/'), '/')::int8[])) \
             LIMIT 1",
            &[&ids],
        )
        .await?;
    if !foreign.is_empty() {
        return Err(Error::validation(refusal));
    }
    Ok(())
}
async fn selected_defaults(tx: &mut Transaction, env: &Env) -> Result<[Vec<i64>; 2]> {
    let ids: Vec<i64> = tx.companies(env).await?.iter().map(|c| c.get()).collect();
    let companies = env.records("res.company")?.browse(ids)?;
    let mut defaults = [BTreeSet::new(), BTreeSet::new()];
    for company in companies.iter() {
        for (i, (_, field)) in FIELDS.iter().enumerate() {
            match company.cell(tx, field).await? {
                Cell::Id(id) => {
                    defaults[i].insert(id);
                }
                Cell::Null => {}
                other => {
                    return Err(Error::programming(format!(
                        "Invalid company tax default: {other:?}"
                    )));
                }
            }
        }
    }
    for (i, field) in [
        "effective_account_sale_tax_id",
        "effective_account_purchase_tax_id",
    ]
    .iter()
    .enumerate()
    {
        if defaults[i].is_empty() {
            for company in companies.iter() {
                match company.cell(tx, field).await? {
                    Cell::Id(id) => {
                        defaults[i].insert(id);
                    }
                    Cell::Null => {}
                    other => {
                        return Err(Error::programming(format!(
                            "Invalid effective tax default: {other:?}"
                        )));
                    }
                }
            }
        }
    }
    let mut result = [Vec::new(), Vec::new()];
    for (out, ids) in result.iter_mut().zip(defaults) {
        // Accepted P4': store no unreadable inherited tax and expose no foreign row.
        *out = env
            .records("account.tax")?
            .browse(ids)?
            .filtered_access(tx, Operation::Read)
            .await?
            .ids();
    }
    Ok(result)
}
