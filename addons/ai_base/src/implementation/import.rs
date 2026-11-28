// SPDX-License-Identifier: MIT OR Apache-2.0
//! Receipts for successful guided imports started from an implementation step.
use super::{Coverage, coverage, same_company};
use erp_sdk::{Operation, prelude::*};
use sha2::{Digest, Sha256};

#[model(name = "ai.implementation.import", description = "Implementation Import Receipt",
    rec_name = "model_name", order = "id desc", company_access = parent(edge = "step_id"),
    constraints = [("_step_contents", "UNIQUE(step_id, fingerprint)", "This import has already been applied to the implementation step.")])]
pub struct ImportReceipt {
    #[field(
        many2one,
        comodel = "ai.implementation.step",
        required = true,
        index = true,
        ondelete = "cascade"
    )]
    pub step_id: Option<RecId>,
    #[field(char, required = true, size = 128, string = "Model")]
    pub model_name: Option<Arc<str>>,
    #[field(char, required = true, size = 64)]
    pub fingerprint: Option<Arc<str>>,
    #[field(integer, string = "Imported records", required = true)]
    pub record_count: i32,
}

fn add_string(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value.as_bytes());
}

/// Identify the normalized model, mapped fields and imported cell values. Labels,
/// CSV delimiters and Excel formatting do not affect the resulting ORM operation.
pub fn import_fingerprint(model: &str, fields: &[&str], rows: &[Vec<String>]) -> String {
    import_fingerprint_iter(
        model,
        fields,
        rows.iter().map(|row| row.iter().map(String::as_str)),
    )
}

/// Hash borrowed strings from either the parsed file or the JSON-2 request without copying cells.
pub fn import_fingerprint_iter<'s, R, P>(model: &str, fields: &[&str], rows: R) -> String
where
    R: IntoIterator<Item = P>,
    R::IntoIter: ExactSizeIterator,
    P: IntoIterator<Item = &'s str>,
    P::IntoIter: ExactSizeIterator,
{
    let rows = rows.into_iter();
    let mut hash = Sha256::new();
    hash.update(b"ERPForAll implementation import v1");
    add_string(&mut hash, model);
    hash.update((fields.len() as u64).to_be_bytes());
    for &field in fields {
        add_string(&mut hash, field);
    }
    hash.update((rows.len() as u64).to_be_bytes());
    for row in rows {
        let row = row.into_iter();
        hash.update((row.len() as u64).to_be_bytes());
        for value in row {
            add_string(&mut hash, value);
        }
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Held for the duration of the ordinary guided import transaction. Neither an input
/// spreadsheet nor its underlying record values are persisted in the receipt.
pub struct StepImport {
    step_id: i64,
    model: String,
    fingerprint: String,
}

impl StepImport {
    /// Resolve the optional source step only after the loader checks target-model access and policy.
    ///
    /// # Errors
    /// A missing or unreadable step, a different company/model or an existing receipt refuses the import.
    pub async fn prepare(
        tx: &mut Transaction,
        model: &str,
        fields: &[&str],
        rows: &[Vec<String>],
    ) -> Result<Option<Self>> {
        let env = tx.env();
        if !env.context().contains("ai_implementation_step_id") {
            return Ok(None);
        }
        Self::prepare_scoped(tx, &env, model, || import_fingerprint(model, fields, rows))
            .await
            .map(Some)
    }

    /// Validate a scoped import against the effective RPC environment and obtain its receipt.
    ///
    /// # Errors
    /// Returns the same access, ownership, domain and duplicate errors as the file importer.
    pub async fn prepare_scoped(
        tx: &mut Transaction,
        env: &Env,
        model: &str,
        fingerprint: impl FnOnce() -> String + Send,
    ) -> Result<Self> {
        let step_id = match env.context().get("ai_implementation_step_id") {
            Some(Value::Int(id)) if id > 0 => id,
            _ => {
                return Err(Error::validation(
                    "Invalid implementation step in import context.",
                ));
            }
        };
        let step = env.records("ai.implementation.step")?.browse([step_id])?;
        step.check_access(tx, Operation::Write).await?;
        step.lock_for_update(tx, true).await?;
        let project = step.related(tx, "project_id").await?;
        same_company(tx, &project).await?;
        let addon = crate::models::text(step.cell(tx, "addon").await?, "Addon")?;
        if !env
            .registry()
            .addons()
            .iter()
            .any(|item| item.name == addon)
            || !matches!(coverage(&addon), Coverage::Scenario(rule) if rule.model == model)
        {
            return Err(Error::validation(
                "The import model does not match the installed domain on this step.",
            ));
        }
        let fingerprint = fingerprint();
        let receipts = env.records("ai.implementation.import")?;
        let domain = Domain::field("step_id")?.eq(step_id)
            & Domain::field("fingerprint")?.eq(fingerprint.as_str());
        if receipts.search_count(tx, &domain, Some(1)).await? > 0 {
            return Err(Error::user(
                "This data and mapping have already been imported. See Imports on the implementation step.",
            ));
        }
        Ok(Self {
            step_id,
            model: model.to_owned(),
            fingerprint,
        })
    }

    /// Record the actual loader outcome in its transaction. Only this method creates
    /// receipts. Interactive users can read receipts for their implementation company.
    ///
    /// # Errors
    /// Any storage failure aborts the same transaction as the import.
    pub async fn complete(self, tx: &mut Transaction, count: usize) -> Result<()> {
        let count = i32::try_from(count)
            .map_err(|_| Error::validation("Import count exceeds its limit."))?;
        let uid = tx.env().user_id().get();
        tx.execute("INSERT INTO ai_implementation_import
            (step_id,model_name,fingerprint,record_count,create_uid,write_uid,create_date,write_date)
            VALUES ($1,$2,$3,$4,$5,$5,now() AT TIME ZONE 'utc',now() AT TIME ZONE 'utc')",
            &[&self.step_id, &self.model, &self.fingerprint, &count, &uid]).await?;
        Ok(())
    }
}
