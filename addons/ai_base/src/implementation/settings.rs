// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reviewed implementation settings delegate to the same company-scoped, revisioned Settings service.
use erp_addon_base::{
    approval_request::{self, Admission},
    ir_config_setting::IrConfigSetting,
};
use erp_sdk::{
    prelude::*,
    rpc::{Effect, Exposure},
    views::wire::{SettingsDescription, SettingsValues},
};
use serde_json::{Value as Json, json};

#[extend(IrConfigSetting, addon = "ai_base")]
pub struct ImplementationSettings {}

/// Only local business configuration. Changes to approval policy, credentials,
/// notifications, global options and module installation belong in native Settings.
fn reviewed(key: &str) -> bool {
    matches!(
        key,
        "account.invoice_receivable_account_code"
            | "account.invoice_payable_account_code"
            | "account.fiscalyear_last_day"
            | "account.fiscalyear_last_month"
            | "account.fiscal_period_horizon_months"
            | "purchase.days_to_purchase"
            | "stock.annual_inventory_month"
            | "stock.annual_inventory_day"
            | "stock.horizon_days"
            | "stock.picking_policy"
            | "stock.replenish_on_order"
    )
}

fn scalar(key: &str, value: &str) -> Result<Json> {
    if !reviewed(key) || value.len() > 512 {
        return Err(Error::validation(
            "Choose a listed company setting and a JSON scalar of at most 512 bytes.",
        ));
    }
    let scalar: Json = serde_json::from_str(value).map_err(|_| {
        Error::validation("Use a JSON string, number or boolean for the setting value.")
    })?;
    if scalar.is_array() || scalar.is_object() || scalar.is_null() {
        return Err(Error::validation(
            "Use a JSON string, number or boolean for the setting value.",
        ));
    }
    Ok(scalar)
}

pub(super) async fn canonical(
    tx: &mut Transaction,
    records: &Records,
    method: &str,
    expected: Effect,
) -> Result<()> {
    let Some(Exposure::Exposed(exposure)) = tx.exposure().get(records.model_id(), method) else {
        return Err(Error::access("The native operation is unavailable."));
    };
    if exposure.effect() != expected
        || records.env().principal().scope().is_some_and(|scope| {
            !scope.models().allows(records.model_name())
                || !scope.methods().allows(method)
                || (scope.read_only() && expected != Effect::Read)
        })
    {
        return Err(Error::access(
            "The native operation is unavailable to this principal.",
        ));
    }
    let call = approval_request::Call {
        model: records.model_name(),
        method,
        effect: expected,
        key: None,
        payload: None,
        binding: None,
    };
    match approval_request::admit(tx, call).await? {
        Admission::Dispatch => Ok(()),
        Admission::KeyRequired => Err(Error::user(
            "Submit this change through its original ERP operation for approval.",
        )),
        Admission::Defer(error) | Admission::ReplayOnly(error) => Err(error),
    }
}

fn company_scope(env: &Env) -> Result<Value> {
    Ok(Value::Json(Arc::new(
        json!({"kind":"company","company_id":super::company(env)?}),
    )))
}

fn decode<T: serde::de::DeserializeOwned>(raw: Value) -> Result<T> {
    let Value::Json(raw) = raw else {
        return Err(Error::invariant("Invalid response from company Settings."));
    };
    serde_json::from_value((*raw).clone()).map_err(|error| {
        Error::invariant("Invalid response from company Settings.").with_source(error)
    })
}

#[methods(IrConfigSetting, addon = "ai_base")]
impl ImplementationSettings {
    /// Return accessible implementation settings and their current company revision.
    #[method]
    #[rpc(effect = read, model, assistant = offer)]
    async fn list_company_setup(
        this: &RecordSet<IrConfigSetting>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let scope = company_scope(this.env())?;
        let records = this.as_records();
        canonical(tx, records, "erp_describe", Effect::Read).await?;
        let description: SettingsDescription =
            decode(records.call(tx, "erp_describe", (scope.clone(),)).await?)?;
        let declarations = description
            .declarations
            .into_iter()
            .filter(|decl| reviewed(&decl.key))
            .collect::<Vec<_>>();
        let keys = declarations
            .iter()
            .map(|decl| decl.key.clone())
            .collect::<Vec<_>>();
        canonical(tx, records, "erp_read_values", Effect::Read).await?;
        let mut values: SettingsValues =
            decode(records.call(tx, "erp_read_values", (scope, keys)).await?)?;
        values.values.retain(|item| reviewed(&item.key));
        Ok(Value::Json(Arc::new(
            json!({"declarations":declarations,"settings":values}),
        )))
    }

    /// Apply a reviewed company setting using the revision read by list_company_setup.
    #[method]
    #[rpc(effect = write, model, assistant = offer)]
    async fn set_company_setup_value(
        this: &RecordSet<IrConfigSetting>,
        tx: &mut Transaction,
        key: String,
        expected_revision: String,
        value_json: String,
    ) -> Result<Value> {
        let value = scalar(&key, &value_json)?;
        if expected_revision.is_empty() || expected_revision.len() > 32 {
            return Err(Error::validation(
                "Read the current company settings revision before changing a value.",
            ));
        }
        let scope = company_scope(this.env())?;
        let records = this.as_records();
        canonical(tx, records, "erp_write_values", Effect::Write).await?;
        let changes = vec![Value::Json(Arc::new(json!({"key":key,"value":value})))];
        let mut applied: SettingsValues = decode(
            records
                .call(tx, "erp_write_values", (scope, expected_revision, changes))
                .await?,
        )?;
        applied.values.retain(|item| reviewed(&item.key));
        Ok(Value::Json(Arc::new(json!(applied))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reviewed_settings_accept_scalars_only_and_exclude_policy_and_egress() {
        assert!(scalar("stock.horizon_days", "17").is_ok());
        assert!(scalar("stock.picking_policy", r#""direct""#).is_ok());
        for input in ["null", "{}", "[]", "", "no"] {
            assert!(scalar("stock.horizon_days", input).is_err());
        }
        for key in [
            "purchase.po_double_validation",
            "stock.stock_text_confirmation",
            "mail.smtp_password",
            "stock.barcode_separator",
        ] {
            assert!(scalar(key, "true").is_err());
        }
    }
}
