// SPDX-License-Identifier: MIT OR Apache-2.0
//! The form/draft default API uses the same company resolution as persisted creation.
use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

pub(super) fn requested(names: &[String], tax: bool) -> Vec<String> {
    let mut requested = names.to_vec();
    if needs_company(names) {
        for field in ["company_id", "country_id", "tax_group_id"] {
            if (tax || field != "tax_group_id") && !requested.iter().any(|n| n == field) {
                requested.push(field.to_owned());
            }
        }
    }
    requested
}
fn needs_company(names: &[String]) -> bool {
    names
        .iter()
        .any(|n| matches!(n.as_str(), "company_id" | "country_id" | "tax_group_id"))
}
pub(super) async fn complete(
    model: &Records,
    tx: &mut Transaction,
    names: &[String],
    mut values: ValueMap,
    tax: bool,
) -> Result<ValueMap> {
    if needs_company(names) {
        let mut company_values = ValueMap::new();
        for field in ["company_id", "country_id", "tax_group_id"] {
            if (tax || field != "tax_group_id")
                && let Some(value) = values.get(field)
            {
                company_values.insert(field.to_owned(), value.clone());
            }
        }
        let mut vals = model.vals_from_values(&company_values)?;
        super::company_defaults(model, tx, &mut vals, tax).await?;
        for field in ["company_id", "country_id", "tax_group_id"] {
            if names.iter().any(|n| n == field)
                && !values.contains_key(field)
                && let Some(cell) = vals.get(field)
            {
                let value = match cell {
                    Cell::Id(id) => Value::Int(*id),
                    Cell::Null => Value::False,
                    other => {
                        return Err(Error::programming(format!(
                            "Invalid tax default {field}: {other:?}"
                        )));
                    }
                };
                values.insert(field.to_owned(), value);
            }
        }
    }
    if tax {
        for (field, document) in [
            ("invoice_repartition_line_ids", "invoice"),
            ("refund_repartition_line_ids", "refund"),
        ] {
            if names.iter().any(|n| n == field) && !values.contains_key(field) {
                values.insert(
                    field.to_owned(),
                    Value::list(["base", "tax"].into_iter().map(|kind| {
                        Command::create(
                            RawVals::new()
                                .set_cell("document_type", Cell::Str(document.into()))
                                .set_cell("repartition_type", Cell::Str(kind.into())),
                        )
                        .to_value()
                    })),
                );
            }
        }
    }
    values.retain(|name, _| names.contains(name));
    Ok(values)
}

/// Resolve declared/context/user defaults using the normal API, with explicit row values first.
pub(super) async fn for_create(
    model: &Records,
    tx: &mut Transaction,
    vals: &mut RawVals,
    tax: bool,
) -> Result<()> {
    let fields = if tax {
        &["company_id", "country_id", "tax_group_id"][..]
    } else {
        &["company_id", "country_id"][..]
    };
    let context = fields.iter().filter_map(|field| {
        vals.get(field)
            .map(|cell| (format!("default_{field}"), Value::from_cell(cell)))
    });
    let defaults_model = model
        .env()
        .with_context(context)
        .records(model.spec().name)?;
    let values = defaults_model.default_get(tx, fields).await?;
    let defaults = model.vals_from_values(&values)?;
    for (field, value) in defaults.iter() {
        if !vals.contains(field) {
            vals.insert(field.to_owned().into(), value.clone());
        }
    }
    Ok(())
}
