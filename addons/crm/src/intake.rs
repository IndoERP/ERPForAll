// SPDX-License-Identifier: MIT OR Apache-2.0
//! Route an enquiry without giving the submitter access to another salesperson's pipeline.
use erp_sdk::prelude::*;
use erp_sdk::{ValueMap, serde_json::json};

#[model(name = "crm.submit.wizard", description = "Submit Lead", kind = "transient",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    transient_max_age_seconds = 7200, transient_max_count = 1000)]
pub struct SubmitLead {
    #[field(char, required = true, size = 256, string = "Lead")]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "cascade",
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    #[field(char, size = 256, string = "Business Name")]
    pub company_name: Option<Arc<str>>,
    #[field(char, size = 256, string = "Contact Name")]
    pub contact_name: Option<Arc<str>>,
    #[field(char, size = 320)]
    pub email: Option<Arc<str>>,
    #[field(char, size = 64)]
    pub phone: Option<Arc<str>>,
    #[field(char, size = 128)]
    pub source: Option<Arc<str>>,
    #[field(char, size = 128)]
    pub region: Option<Arc<str>>,
    #[field(char, size = 512)]
    pub next_step: Option<Arc<str>>,
    #[field(boolean, readonly = true)]
    pub submitted: bool,
}

#[methods(SubmitLead, addon = "crm")]
impl SubmitLead {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<SubmitLead>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let company_requested = fields.iter().any(|f| f == "company_id");
        let mut result: ValueMap = sup.call(tx, this.as_records(), (fields,)).await?;
        if company_requested
            && !result.get("company_id").is_some_and(Value::is_truthy)
            && let Some(company) = this.env().company_id()?
        {
            result.insert("company_id".to_owned(), Value::Int(company));
        }
        Ok(result)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<SubmitLead>,
        tx: &mut Transaction,
        mut vals: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for row in &mut vals {
            // A receipt always starts pending, including callers with custom default_* values.
            row.insert("submitted".into(), Cell::Bool(false));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<SubmitLead>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("submitted") {
            return Err(Error::validation(
                "The Submit action manages the submission receipt.",
            ));
        }
        let drafts = this.as_records();
        drafts.check_access(tx, erp_sdk::Operation::Write).await?;
        drafts.lock_for_update(tx, true).await?;
        if this.mapped(tx, Self::SUBMITTED).await?.contains(&true) {
            return Err(Error::validation(
                "This enquiry has already been submitted.",
            ));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_submit(this: &RecordSet<SubmitLead>, tx: &mut Transaction) -> Result<Value> {
        let wizard = this.ensure_one()?.as_records();
        tx.savepoint(async |tx| {
            crate::lifecycle::locked(tx, wizard).await?;
            if !this.get(tx, Self::SUBMITTED).await? {
                let env = crate::lifecycle::clean_env(this.env())
                    .with_context([("crm_personal_form", Value::Bool(false))]);
                let mut prospect = RawVals::new().set_cell("kind", Cell::Str("lead".into()));
                for field in [
                    "name",
                    "company_id",
                    "company_name",
                    "contact_name",
                    "email",
                    "phone",
                    "source",
                    "region",
                    "next_step",
                ] {
                    let value = wizard.cell(tx, field).await?;
                    if !value.is_null() {
                        prospect.insert(field.into(), value);
                    }
                }
                env.records("crm.lead")?.create(tx, vec![prospect]).await?;
                wizard
                    .write_base(tx, RawVals::new().set_cell("submitted", Cell::Bool(true)))
                    .await?;
            }
            Ok(Value::Json(Arc::new(json!({
                "type":"ir.actions.client","tag":"display_notification",
                "params":{"type":"success","message":"Lead submitted to the sales queue.",
                    "next":{"type":"ir.actions.act_window_close"}}
            }))))
        })
        .await
    }
}
