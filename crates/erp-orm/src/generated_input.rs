// SPDX-License-Identifier: MIT OR Apache-2.0
//! Authorization and normalization before the unchanged generated assignment guard.
use crate::dispatch::{Dispatch, MethodDispatchTarget, MethodFn, call_method};
use crate::{FieldOp, RawVals, Records, Transaction};
use erp_fields::{Error, Result};
use erp_registry::{ModelSpec, Registry};
pub(crate) const METHOD: &str = "_adapt_generated_input";

/// Ordered input to a model's ordinary `_adapt_generated_input` method.
/// Core constructs the request only after authorizing the flagged input names. Returning values
/// does not authorize their assignment: core validates names and the ordinary writer checks them.
///
/// ```
/// use erp_orm::{GeneratedInput, RawVals};
/// fn inspect(input: GeneratedInput) -> Vec<RawVals> {
///     let _create = input.is_create();
///     let _row_count = input.rows().len();
///     input.into_rows()
/// }
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GeneratedInput {
    create: bool,
    rows: Vec<RawVals>,
}
impl GeneratedInput {
    /// Whether the input belongs to create rather than a persisted write.
    pub fn is_create(&self) -> bool {
        self.create
    }
    /// Values in original create order or target-recordset order for write.
    pub fn rows(&self) -> &[RawVals] {
        &self.rows
    }
    /// Take the ordered values for normalization; the result must preserve row positions.
    pub fn into_rows(self) -> Vec<RawVals> {
        self.rows
    }
}

pub(crate) fn validate_chains(registry: &Registry, dispatch: &Dispatch) -> Result<()> {
    for model in registry
        .models()
        .iter()
        .filter(|m| m.fields.iter().any(|f| f.is_input_adapted()))
    {
        let valid = dispatch.chain(model.id, METHOD).is_some_and(|chain| {
            chain.targets().iter().all(|entry| match entry {
                MethodDispatchTarget::Native(method) => {
                    (method.entry)().is::<MethodFn<(GeneratedInput,), Vec<RawVals>>>()
                }
                MethodDispatchTarget::Wasm(_) => false,
            })
        });
        if !valid {
            return Err(Error::programming(format!(
                "Model '{}' requires native _adapt_generated_input(GeneratedInput) -> Vec<RawVals>",
                model.name
            )));
        }
    }
    Ok(())
}

fn has_input(model: &ModelSpec, rows: &[RawVals]) -> bool {
    rows.iter().any(|row| {
        row.names()
            .any(|name| model.field(name).is_some_and(|f| f.is_input_adapted()))
    })
}
fn checked_output(model: &ModelSpec, before: &[RawVals], after: &[RawVals]) -> Result<()> {
    let fail = |reason: &str| {
        Error::programming(format!(
            "Generated input adapter on '{}': {reason}",
            model.name
        ))
    };
    if before.len() != after.len() {
        return Err(fail("output row count changed"));
    }
    for (input, output) in before.iter().zip(after) {
        let flagged: Vec<_> = input
            .names()
            .filter_map(|name| model.field(name).filter(|f| f.is_input_adapted()))
            .collect();
        if flagged.is_empty() {
            if input != output {
                return Err(fail("a row without adapted input changed"));
            }
            continue;
        }
        for name in output.names() {
            let Some(field) = model.field(name) else {
                return Err(fail(&format!("output contains unknown field '{name}'")));
            };
            if field.is_generated() {
                return Err(fail(&format!("output retains generated field '{name}'")));
            }
            if input.names().any(|original| original == name) {
                continue;
            }
            let source = field.inherited.is_none()
                && field.is_assignment_eligible()
                && field.has_column()
                && flagged.iter().any(|f| {
                    f.generated()
                        .is_some_and(|g| g.dependencies().contains(&field.id))
                });
            if !source {
                return Err(fail(&format!("output adds unrelated field '{name}'")));
            }
        }
    }
    Ok(())
}

fn adapt<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    rows: Vec<RawVals>,
    create: bool,
) -> crate::BoxFuture<'a, Result<Vec<RawVals>>> {
    // Erase only the adapted path: access diagnostics can read records and re-enter dispatch.
    Box::pin(async move {
        let model = records.spec();
        tx.refuse_adapter_effect()?;
        // Unflagged generated inputs cannot be consumed by an adapter to evade the ordinary guard.
        crate::database_generated::check_write_names(
            tx,
            model,
            rows.iter()
                .flat_map(|row| row.names())
                .filter(|name| !model.field(name).is_some_and(|f| f.is_input_adapted())),
        )?;
        tx.prepare_access(records.env()).await?;
        tx.check_fields(
            records.env(),
            model.id,
            rows.iter().flat_map(|row| row.names()).filter_map(|name| {
                model
                    .field(name)
                    .filter(|f| f.is_input_adapted())
                    .map(|f| f.id)
            }),
            FieldOp::Write,
        )?;
        records
            .check_access(
                tx,
                if create {
                    crate::Operation::Create
                } else {
                    crate::Operation::Write
                },
            )
            .await?;
        // A callback may remove input names, but cannot hide an invalid original name.
        for name in rows.iter().flat_map(|row| row.names()) {
            if model.field(name).is_none() {
                return Err(Error::programming(format!(
                    "Invalid field '{name}' in '{}'",
                    model.name
                )));
            }
        }
        let input = GeneratedInput {
            create,
            rows: rows.clone(),
        };
        let mut scope = tx.generated_input_scope(records)?;
        let result =
            call_method::<(GeneratedInput,), Vec<RawVals>>(&mut scope, records, METHOD, (input,))
                .await;
        scope.finish()?;
        let output = result?;
        checked_output(model, &rows, &output)?;
        Ok(output)
    })
}

pub(crate) async fn create(
    tx: &mut Transaction,
    records: &Records,
    rows: Vec<RawVals>,
) -> Result<Vec<RawVals>> {
    crate::database_generated::check_write_names(
        tx,
        records.spec(),
        records.env().context().default_field_names(),
    )?;
    if !has_input(records.spec(), &rows) {
        return Ok(rows);
    }
    adapt(tx, records, rows, true).await
}

/// None retains the original, uniform write path without cloning the input.
pub(crate) async fn write(
    tx: &mut Transaction,
    records: &Records,
    values: &RawVals,
) -> Result<Option<Vec<RawVals>>> {
    if !has_input(records.spec(), std::slice::from_ref(values)) {
        return Ok(None);
    }
    if records.is_empty() || records.rec_ids().iter().any(|id| id.is_new()) {
        crate::database_generated::check_write_names(tx, records.spec(), values.names())?;
    }
    adapt(tx, records, vec![values.clone(); records.len()], false)
        .await
        .map(Some)
}
