// SPDX-License-Identifier: MIT OR Apache-2.0
//! Private contextual label composition and reader-authorized supplier batches.
use erp_sdk::prelude::*;

pub(crate) fn context_flag(env: &Env, key: &str, default: bool) -> Result<bool> {
    match env.context().get(key) {
        None => Ok(default),
        Some(Value::Bool(v)) => Ok(v),
        Some(Value::False) => Ok(false),
        Some(_) => Err(Error::validation(format!(
            "Product label context {key} must be a boolean"
        ))),
    }
}

pub(crate) fn format_label(env: &Env, name: &str, code: Option<&str>) -> Result<String> {
    let display = context_flag(env, "display_default_code", true)?;
    let formatted = context_flag(env, "formatted_display_name", false)?;
    Ok(match code.filter(|code| display && !code.is_empty()) {
        Some(code) if formatted => format!("{name}\t--{code}--"),
        Some(code) => format!("[{code}] {name}"),
        None => name.to_owned(),
    })
}

/// One reader-authorized vendor row in the model's declared order.
pub(crate) struct Seller {
    pub(crate) id: i64,
    pub(crate) template: i64,
    pub(crate) variant: Option<i64>,
    pub(crate) partner: i64,
    pub(crate) company: Option<i64>,
    pub(crate) name: Option<Arc<str>>,
    pub(crate) code: Option<Arc<str>>,
}

pub(crate) fn context_id(env: &Env, key: &str) -> Result<Option<i64>> {
    match env.context().get(key) {
        None | Some(Value::False | Value::Bool(false)) => Ok(None),
        Some(Value::Int(id)) if id > 0 => Ok(Some(id)),
        Some(_) => Err(Error::validation(format!(
            "Product label context {key} must be a positive record ID or false"
        ))),
    }
}

/// Supplier queries always run as the original consumer, including indirect elevated labels.
/// An empty supplier set is the specified public-label fallback for denied supplier access.
pub(crate) async fn sellers(
    env: &Env,
    tx: &mut Transaction,
    templates: &[i64],
) -> Result<Vec<Seller>> {
    use crate::supplierinfo::ProductSupplierinfo;
    use erp_sdk::Operation;
    let partner = context_id(env, "partner_id")?;
    let explicit = context_id(env, "seller_id")?;
    if templates.is_empty() || (partner.is_none() && explicit.is_none()) {
        return Ok(Vec::new());
    }
    let reader = env.label_reader();
    let model = reader.records("product.supplierinfo")?;
    if !model.has_access(tx, Operation::Read).await? {
        return Ok(Vec::new());
    }
    let mut partners: Vec<i64> = partner.into_iter().collect();
    if let Some(partner) = partner {
        let rec = reader.records("res.partner")?.browse([partner])?;
        if rec.has_access(tx, Operation::Read).await? {
            match rec.cell(tx, "commercial_partner_id").await? {
                Cell::Id(id) => {
                    if !partners.contains(&id) {
                        partners.push(id);
                    }
                }
                Cell::Null => {}
                other => {
                    return Err(Error::programming(format!(
                        "res.partner.commercial_partner_id returned {other:?}"
                    )));
                }
            }
        }
    }
    // with_user may retain an unresolved Env; use the transaction's validated default companies.
    let company_ids: Vec<i64> = tx
        .companies(reader)
        .await?
        .iter()
        .map(|id| id.get())
        .collect();
    let companies = Domain::condition("company_id", Operator::Eq, false)?
        | Domain::condition("company_id", Operator::In, company_ids)?;
    let requested = Domain::condition("partner_id", Operator::In, partners)?;
    let requested = match explicit {
        Some(id) => requested | Domain::condition("id", Operator::Eq, id)?,
        None => requested,
    };
    let domain = Domain::condition("product_tmpl_id", Operator::In, templates.to_vec())?
        & companies
        & requested;
    let found = model
        .search_with(
            tx,
            &domain,
            &SearchOptions::new().fetch_fields([
                "product_tmpl_id",
                "product_id",
                "partner_id",
                "company_id",
                "product_name",
                "product_code",
            ]),
        )
        .await?;
    let found = RecordSet::<ProductSupplierinfo>::try_from(found)?;
    let templates = found
        .mapped(tx, ProductSupplierinfo::PRODUCT_TMPL_ID)
        .await?;
    let variants = found.mapped(tx, ProductSupplierinfo::PRODUCT_ID).await?;
    let partners = found.mapped(tx, ProductSupplierinfo::PARTNER_ID).await?;
    let companies = found.mapped(tx, ProductSupplierinfo::COMPANY_ID).await?;
    let names = found.mapped(tx, ProductSupplierinfo::PRODUCT_NAME).await?;
    let codes = found.mapped(tx, ProductSupplierinfo::PRODUCT_CODE).await?;
    let mut rows = Vec::with_capacity(found.len());
    for index in 0..found.len() {
        rows.push(Seller {
            id: found.as_records().rec_ids()[index].get(),
            template: templates[index]
                .ok_or_else(|| Error::programming("Vendor label row has no product template"))?
                .get(),
            variant: variants[index].map(RecId::get),
            partner: partners[index]
                .ok_or_else(|| Error::programming("Vendor label row has no partner"))?
                .get(),
            company: companies[index].map(RecId::get),
            name: names[index].clone().filter(|v| !v.is_empty()),
            code: codes[index].clone().filter(|v| !v.is_empty()),
        });
    }
    Ok(rows)
}

/// Delegated template names, including draft parents, in the variant set's order.
pub(crate) async fn template_names(
    this: &RecordSet<crate::product::ProductProduct>,
    tx: &mut Transaction,
) -> Result<Vec<Option<Arc<str>>>> {
    use crate::product::ProductProduct;
    use crate::template::ProductTemplate;
    let ids = this
        .mapped(tx, ProductProduct::PRODUCT_TMPL_ID)
        .await?
        .into_iter()
        .map(|id| id.ok_or_else(|| Error::validation("Product variant missing template link")))
        .collect::<Result<Vec<_>>>()?;
    let parents = RecordSet::<ProductTemplate>::try_from(
        this.env().records("product.template")?.browse_ids(ids),
    )?;
    parents.mapped(tx, ProductTemplate::NAME).await
}
