//! Authoritative named digits for mutation admission (approved L-9).
//! Metadata overrides are deliberately separate from this transaction-visible store cache.
use crate::{Env, Transaction};
use erp_fields::convert::{FieldRef, validate_named_scale};
use erp_fields::{Cell, Digits, Error, FieldType, Result};
use erp_registry::{FieldSpec, ModelSpec};

pub(crate) fn usage(field: &FieldSpec) -> Option<&'static str> {
    match field.ty {
        FieldType::Float {
            digits: Some(Digits::Named(usage)),
        } => Some(usage),
        _ => None, // Other kinds have their own conversion and rounding contracts.
    }
}

pub(crate) async fn resolve(tx: &mut Transaction, env: &Env, usage: &str) -> Result<u32> {
    if usage.is_empty() || usage.len() > 128 {
        return Err(Error::validation(
            "A decimal precision usage must be 1 to 128 bytes long.",
        ));
    }
    if let Some(scale) = tx.engine.write_precisions.get(usage) {
        return Ok(*scale);
    }
    let scale = if tx.registry().model("decimal.precision").is_some() {
        Box::pin(env.records("decimal.precision")?.call::<(String,), u32>(
            tx,
            "_precision_get",
            (usage.to_owned(),),
        ))
        .await?
    } else {
        // / FIELD-022: absence of the model means no usage can be configured.
        2
    };
    if scale > 28 {
        return Err(Error::validation(format!(
            "Decimal precision '{usage}' must be between 0 and 28, got {scale}."
        )));
    }
    tx.engine.write_precisions.insert(usage.to_owned(), scale);
    Ok(scale)
}

pub(crate) async fn validate(
    tx: &mut Transaction,
    env: &Env,
    spec: &ModelSpec,
    field: &FieldSpec,
    cell: &Cell,
) -> Result<()> {
    let Some(usage) = usage(field) else {
        return Ok(()); // Non-named fields retain their own conversion contracts.
    };
    let value = match cell {
        Cell::Null => return Ok(()), // Required-field validation owns null admission.
        Cell::Decimal(value) => value,
        _ => {
            return Err(Error::programming(format!(
                "Named precision field {}.{} requires a Decimal or Null cell.",
                spec.name, field.name
            )));
        }
    };
    let digits = resolve(tx, env, usage).await?;
    validate_named_scale(
        FieldRef {
            model: spec.name,
            field: field.name,
            label: &field.string,
        },
        usage,
        *value,
        digits,
    )?;
    Ok(())
}

pub(crate) async fn rows(
    tx: &mut Transaction,
    env: &Env,
    spec: &ModelSpec,
    rows: &[Vec<Option<Cell>>],
) -> Result<()> {
    for field in spec.fields.iter().filter(|f| f.store && usage(f).is_some()) {
        for row in rows {
            if let Some(cell) = &row[field.id.index()] {
                validate(tx, env, spec, field, cell).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use erp_fields::ErrorKind;
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
    use std::sync::Arc;

    #[tokio::test]
    async fn named_cell_type_is_checked_before_precision_lookup() {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "precision_boundary",
            depends: &[],
            exact_only: false,
        };
        static ITEM: ModelDecl = ModelDecl::new("precision_boundary", "precision.item").fields(&[
            FieldDecl::float_digits("price", Digits::Named("Product Price")),
            FieldDecl::boolean("active"),
        ]);
        // The installed provider model intentionally has no adapter: wrong-shaped cells
        // must fail before attempting that service; Null must not resolve it either.
        static PRECISION: ModelDecl = ModelDecl::new("precision_boundary", "decimal.precision");
        let registry = Arc::new(
            RegistryBuilder::new([&ADDON])
                .linked(false)
                .with_decls([&ITEM, &PRECISION])
                .build()
                .unwrap(),
        );
        let config = std::env::var("DATABASE_URL")
            .expect("test database endpoint")
            .parse()
            .unwrap();
        let pool = crate::pool::create_pool(config, 1).unwrap();
        let orm = crate::Orm::builder(Arc::clone(&registry), pool)
            .policy(crate::StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let mut tx = orm.begin(crate::SUPERUSER_ID).await.unwrap();
        let env = tx.env();
        let spec = registry.model("precision.item").unwrap();
        let field = spec.field("price").unwrap();
        for cell in [Cell::Bool(true), Cell::Int(1)] {
            let error = validate(&mut tx, &env, spec, field, &cell)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "Named precision field precision.item.price requires a Decimal or Null cell."
            );
        }
        validate(&mut tx, &env, spec, field, &Cell::Null)
            .await
            .unwrap();
        validate(
            &mut tx,
            &env,
            spec,
            spec.field("active").unwrap(),
            &Cell::Bool(true),
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
    }
}
