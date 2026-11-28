// SPDX-License-Identifier: MIT OR Apache-2.0
//! Small contact master-data operations for implementation sessions. The normal
//! partner create/write chain still owns field checks, business rules and side effects.
use super::{company, settings::canonical};
use erp_addon_base::res_partner::ResPartner;
use erp_sdk::{Operation, ValueMap, prelude::*, rpc::Effect};
use serde_json::json;

/// An immutable creation identity. The partner's everyday reference stays editable,
/// and other contact creators continue to use the native model unchanged.
#[model(name="ai.implementation.contact.key",description="Setup Contact Creation Key",
    company_access=direct(field="company_id",branch="exact",empty="deny"),
    constraints=[("_company_reference","UNIQUE (company_id, reference)","This contact creation reference was already used in this company.")])]
struct ContactKey {
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "cascade"
    )]
    company_id: Option<RecId>,
    #[field(char, required = true, size = 128)]
    reference: Option<Arc<str>>,
    #[field(many2one, comodel = "res.partner", ondelete = "set null", index = true)]
    partner_id: Option<RecId>,
}

#[extend(ResPartner, addon = "ai_base")]
pub struct ContactSetup {}

fn text(value: &str, max: usize, required: bool) -> Result<&str> {
    let value = value.trim();
    if (required && value.is_empty()) || value.len() > max || value.chars().any(char::is_control) {
        return Err(Error::validation(
            "Contact data is empty, contains a control character or exceeds its field limit.",
        ));
    }
    Ok(value)
}

fn email_value(email: &str) -> Result<&str> {
    let email = text(email, 254, false)?;
    if !email.is_empty()
        && (!email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        }) || email.chars().any(char::is_whitespace))
    {
        return Err(Error::validation(
            "Enter one email address or leave it empty.",
        ));
    }
    Ok(email)
}

#[methods(ResPartner, addon = "ai_base")]
impl ContactSetup {
    /// Create a contact in the active company. A stable business reference prevents duplicate retries.
    #[method]
    #[rpc(effect = write, model, assistant = offer)]
    async fn create_setup_contact(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        name: String,
        reference: String,
        email: String,
        is_company: bool,
    ) -> Result<Value> {
        let name = text(&name, 160, true)?;
        let reference = text(&reference, 128, true)?;
        let email = email_value(&email)?;
        let scope = company(this.env())?;
        let records = this.as_records();
        canonical(tx, records, "create", Effect::Write).await?;
        records.check_access(tx, Operation::Create).await?;
        if this.env().context().has_default_keys() {
            return Err(Error::validation(
                "Contact setup accepts only the fields in its request.",
            ));
        }
        let domain = Domain::field("company_id")?.eq(scope) & Domain::field("ref")?.eq(reference);
        let matches = records
            .search_with(
                tx,
                &domain,
                &SearchOptions::new().active_test(false).limit(1),
            )
            .await?;
        if !matches.is_empty() {
            return Err(Error::validation(
                "A contact with this reference already exists in this company. Open it before making changes.",
            ));
        }
        let values = ValueMap::from([
            ("name".into(), Value::str(name)),
            ("ref".into(), Value::str(reference)),
            ("email".into(), Value::str(email)),
            ("is_company".into(), Value::Bool(is_company)),
            ("company_id".into(), Value::Int(scope)),
        ]);
        let vals = records.vals_from_values(values.iter())?;
        let id = tx.savepoint(async |tx| {
            let uid = records.env().user_id().get();
            let keys = tx.query(
                "INSERT INTO ai_implementation_contact_key
                   (company_id, reference, create_uid, write_uid, create_date, write_date)
                 VALUES ($1, $2, $3, $3, now() AT TIME ZONE 'utc', now() AT TIME ZONE 'utc')
                 ON CONFLICT (company_id, reference) DO NOTHING RETURNING id",
                &[&scope, &reference, &uid],
            ).await?;
            let Some(key) = keys.first() else {
                return Err(Error::validation("This setup reference already exists in this company. Review the original contact before making changes."));
            };
            let key: i64 = key.try_get(0).map_err(|error| Error::db("Invalid contact creation key.").with_source(error))?;
            let id = records.create(tx, vec![vals]).await?.ensure_one()?.int()?;
            if tx.execute("UPDATE ai_implementation_contact_key SET partner_id=$2 WHERE id=$1", &[&key,&id]).await? != 1 {
                return Err(Error::invariant("The contact creation key changed during setup."));
            }
            Ok(id)
        }).await?;
        Ok(Value::Json(Arc::new(
            json!({"type":"ir.actions.act_window","name":"Contact","res_model":"res.partner","res_id":id,"views":[[false,"form"]],"target":"current"}),
        )))
    }

    /// Change one company contact's email only when its prior address still matches.
    #[method]
    #[rpc(effect = write, assistant = offer)]
    async fn update_setup_contact_email(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        expected_email: String,
        new_email: String,
    ) -> Result<bool> {
        let new_email = email_value(&new_email)?;
        if expected_email.len() > 254 {
            return Err(Error::validation(
                "Enter the contact's current email address.",
            ));
        }
        let contact = this.ensure_one()?.as_records();
        canonical(tx, contact, "write", Effect::Write).await?;
        contact.check_access(tx, Operation::Read).await?;
        contact.check_access(tx, Operation::Write).await?;
        contact.lock_for_update(tx, true).await?;
        if contact.cell(tx, "company_id").await? != Cell::Id(company(this.env())?) {
            return Err(Error::access(
                "Select a contact belonging to the active company.",
            ));
        }
        let current = contact.cell(tx, "email").await?;
        let unchanged = match current {
            Cell::Null => expected_email.is_empty(),
            Cell::Str(stored) => stored.as_ref() == expected_email,
            _ => return Err(Error::invariant("Invalid contact email.")),
        };
        if !unchanged {
            return Err(Error::validation(
                "The contact email changed. Read its current value and review the update.",
            ));
        }
        let values = ValueMap::from([("email".into(), Value::str(new_email))]);
        contact.write_values(tx, values.iter()).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn single_email_and_reference_have_bounded_inputs() {
        assert_eq!(
            email_value(" sales@example.test ").ok(),
            Some("sales@example.test")
        );
        assert_eq!(email_value("").ok(), Some(""));
        for value in ["a@", "@foo", "a@b@c", "a b@c", "a@b\nCc: c@d"] {
            assert!(email_value(value).is_err());
        }
        assert!(text("", 128, true).is_err());
        assert!(text(&"x".repeat(129), 128, true).is_err());
    }
}
