// SPDX-License-Identifier: MIT OR Apache-2.0
//! Qualification reuses the sales record and its conversation. Matching only searches partners visible to this user.
use crate::{CrmLead, lifecycle, pipeline};
use erp_sdk::prelude::*;
use erp_sdk::{Operation, serde_json::json};

erp_sdk::model_ref!(pub Partner = "res.partner" { email: Option<Arc<str>>, phone: Option<Arc<str>> });

#[extend(Partner, addon = "crm")]
pub struct PartnerMatching {
    /// Explicitly identifies a CRM business account even before it has a tax registration.
    #[field(boolean, string = "CRM Business Account")]
    pub crm_is_business: bool,
    #[field(
        integer,
        compute = "_compute_crm_opportunity_count",
        groups = "sales_team.group_sale_salesman",
        string = "Opportunities"
    )]
    pub crm_opportunity_count: i32,
    #[field(
        char,
        compute = "_compute_crm_match_keys",
        store = true,
        index = "btree_not_null",
        string = "CRM Email Match"
    )]
    pub crm_email_key: Option<Arc<str>>,
    #[field(
        char,
        compute = "_compute_crm_match_keys",
        store = true,
        index = "btree_not_null",
        string = "CRM Phone Match"
    )]
    pub crm_phone_key: Option<Arc<str>>,
}

fn email_key(input: Option<&str>) -> Option<Arc<str>> {
    input
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase().into())
}
fn phone_key(input: Option<&str>) -> Option<Arc<str>> {
    let digits: String = input?.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.strip_prefix("00").unwrap_or(&digits);
    (digits.len() >= 6).then(|| digits.into())
}

#[methods(Partner, addon = "crm")]
impl PartnerMatching {
    #[compute(depends = ["company_id"], depends_context = ["uid","allowed_company_ids"])]
    async fn _compute_crm_opportunity_count(
        this: &RecordSet<Partner>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Partner>,
    ) -> Result<()> {
        let opportunities = this.env().records("crm.lead")?;
        let kind = Domain::condition("kind", Operator::Eq, "opportunity")?;
        for (index, id) in this.as_records().rec_ids().iter().enumerate() {
            let count = if id.is_new() {
                0
            } else {
                let related = Domain::condition("partner_id", Operator::Eq, id.get())?
                    | Domain::condition("contact_role_ids.partner_id", Operator::Eq, id.get())?;
                i32::try_from(
                    opportunities
                        .search_count(tx, &(kind.clone() & related), None)
                        .await?,
                )
                .map_err(|_| Error::validation("Too many opportunities to count."))?
            };
            out.set(index, Self::CRM_OPPORTUNITY_COUNT, count)?;
        }
        Ok(())
    }
    #[compute(depends = ["email","phone"])]
    fn _compute_crm_match_keys(batch: &mut Batch<'_, Partner>) -> Result<()> {
        for i in batch.indices() {
            let email = batch.get(i, Partner::EMAIL)?;
            let phone = batch.get(i, Partner::PHONE)?;
            batch.set(i, Self::CRM_EMAIL_KEY, email_key(email.as_deref()))?;
            batch.set(i, Self::CRM_PHONE_KEY, phone_key(phone.as_deref()))?;
        }
        Ok(())
    }
    #[method]
    #[rpc(effect = read)]
    async fn action_view_crm_opportunities(
        this: &RecordSet<Partner>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let partner = this.ensure_one()?.as_records();
        partner.check_access(tx, Operation::Read).await?;
        this.env()
            .records("crm.lead")?
            .check_access(tx, Operation::Read)
            .await?;
        let id = partner.int()?;
        Ok(Value::Json(Arc::new(
            json!({"type":"ir.actions.act_window","name":"Opportunities","res_model":"crm.lead","target":"current","views":[[false,"list"],[false,"form"]],"domain":["&",["kind","=","opportunity"],"|",["partner_id","=",id],["contact_role_ids.partner_id","=",id]],"context":{"default_kind":"opportunity","default_partner_id":id}}),
        )))
    }
}

#[model(name = "crm.lead.contact", description = "Deal Contact", rec_name = "partner_id",
company_access = parent(edge = "lead_id"),constraints = [("_role_unique","unique(lead_id,partner_id,role)","This person already holds that role on this deal.")])]
pub struct ContactRole {
    #[field(
        many2one,
        comodel = "crm.lead",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Deal"
    )]
    pub lead_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Contact"
    )]
    pub partner_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.users",
        related = "lead_id.user_id",
        store = true,
        string = "Salesperson"
    )]
    pub owner_id: Option<RecId>,
    #[field(selection, selection = [("decision","Decision Maker"),("user","End User"),("purchasing","Purchasing"),("influencer","Influencer"),("other","Other")], default = "other", required = true, string = "Contact Role")]
    pub role: Option<Arc<str>>,
}

#[methods(ContactRole, addon = "crm")]
impl ContactRole {
    #[constrains("lead_id", "partner_id")]
    async fn _check_parties(this: &RecordSet<ContactRole>, tx: &mut Transaction) -> Result<()> {
        for link in this.as_records().iter() {
            let lead = link.related(tx, "lead_id").await?;
            lead.check_access(tx, Operation::Write).await?;
            let Cell::Id(company) = lead.cell(tx, "company_id").await? else {
                return Err(Error::invariant("A deal needs a company."));
            };
            let person = link.related(tx, "partner_id").await?;
            pipeline::assert_company(tx, &person, company).await?;
        }
        Ok(())
    }
}

#[model(name = "crm.qualify.wizard", description = "Qualify Lead", kind = "transient", company_access = parent(edge = "lead_id"),transient_max_age_seconds = 7200, transient_max_count = 1000)]
pub struct QualifyWizard {
    #[field(
        many2one,
        comodel = "crm.lead",
        required = true,
        readonly = true,
        string = "Lead"
    )]
    pub lead_id: Option<RecId>,
    #[field(selection, selection = [("existing","Use Existing Customer"),("person","Create Individual Customer"),("business","Create Business Customer")], required = true, default = "existing", string = "Customer")]
    pub choice: Option<Arc<str>>,
    #[field(many2one, comodel = "res.partner", string = "Existing Customer")]
    pub partner_id: Option<RecId>,
    #[field(
        many2many,
        comodel = "res.partner",
        relation = "crm_qualify_candidate_rel",
        column1 = "wizard_id",
        column2 = "partner_id",
        readonly = true,
        string = "Matching Contacts"
    )]
    pub candidate_ids: Vec<RecId>,
    #[field(char, size = 256, string = "Company Name")]
    pub company_name: Option<Arc<str>>,
    #[field(char, size = 256, string = "Contact Name")]
    pub contact_name: Option<Arc<str>>,
}

async fn matching(tx: &mut Transaction, lead: &Records, company: i64) -> Result<Vec<i64>> {
    let email = match lead.cell(tx, "email").await? {
        Cell::Str(s) => email_key(Some(&s)),
        _ => None,
    };
    let phone = match lead.cell(tx, "phone").await? {
        Cell::Str(s) => phone_key(Some(&s)),
        _ => None,
    };
    let mut any = None;
    for (key, value) in [("crm_email_key", email), ("crm_phone_key", phone)] {
        if let Some(value) = value {
            let term = Domain::condition(key, Operator::Eq, value.to_string())?;
            any =
                Some(any.map_or_else(|| term.clone(), |previous: Domain| previous | term.clone()));
        }
    }
    let Some(any) = any else {
        return Ok(Vec::new());
    };
    let scope = Domain::condition("company_id", Operator::Eq, company)?
        | Domain::condition("company_id", Operator::Eq, false)?;
    Ok(lead
        .env()
        .records("res.partner")?
        .search_with(tx, &(any & scope), &erp_sdk::SearchOptions::new().limit(25))
        .await?
        .ids())
}

fn form_action(model: &str, id: i64, name: &str, target: &str) -> Value {
    Value::Json(Arc::new(
        json!({"type":"ir.actions.act_window","res_model":model,"name":name,"res_id":id,"target":target,"views":[[false,"form"]]}),
    ))
}

#[methods(CrmLead, addon = "crm")]
impl CrmLead {
    #[method]
    #[rpc(effect = write, assistant = offer)]
    async fn action_qualify(this: &RecordSet<CrmLead>, tx: &mut Transaction) -> Result<Value> {
        let lead = this.ensure_one()?.as_records();
        lead.check_access(tx, Operation::Write).await?;
        if lead.cell(tx, "kind").await? != Cell::Str("lead".into())
            || lead.cell(tx, "state").await? != Cell::Str("open".into())
        {
            return Err(Error::user("Only an open lead can be qualified."));
        }
        let Cell::Id(company) = lead.cell(tx, "company_id").await? else {
            return Err(Error::invariant("The lead needs a company."));
        };
        let candidates = matching(tx, lead, company).await?;
        let links = candidates
            .iter()
            .map(|id| {
                RecId::from_real(*id)
                    .map(Command::link)
                    .ok_or_else(|| Error::invariant("Invalid contact ID."))
            })
            .collect::<Result<Vec<_>>>()?;
        let chosen = match lead.cell(tx, "partner_id").await? {
            Cell::Id(id) => Some(id),
            _ => match candidates.as_slice() {
                [id] => Some(*id),
                _ => None,
            },
        };
        let business = lead.cell(tx, "company_name").await?;
        let choice = if chosen.is_some() || !candidates.is_empty() {
            "existing"
        } else if business.is_null() {
            "person"
        } else {
            "business"
        };
        let mut vals = RawVals::new()
            .set_cell("lead_id", Cell::Id(lead.int()?))
            .set_cell("choice", Cell::Str(choice.into()))
            .set_cell("company_name", business)
            .set_cell("contact_name", lead.cell(tx, "contact_name").await?)
            .set_commands("candidate_ids", links);
        if let Some(id) = chosen {
            vals.insert("partner_id".into(), Cell::Id(id));
        }
        let wizard = lifecycle::clean_env(this.env())
            .records("crm.qualify.wizard")?
            .create(tx, vec![vals])
            .await?;
        Ok(form_action(
            "crm.qualify.wizard",
            wizard.int()?,
            "Qualify Lead",
            "new",
        ))
    }
}

fn nonblank(cell: Cell, label: &str) -> Result<Cell> {
    if matches!(&cell,Cell::Str(s) if !s.trim().is_empty()) {
        Ok(cell)
    } else {
        Err(Error::validation(format!("Enter a {label}.")))
    }
}

/// Resolve a buyer to their business account while retaining their role on the opportunity.
async fn existing_customer(
    tx: &mut Transaction,
    selected: Records,
    company: i64,
) -> Result<(Records, Option<i64>)> {
    if selected.is_empty() {
        return Err(Error::validation("Choose a customer or create one."));
    }
    pipeline::assert_company(tx, &selected, company).await?;
    let account = selected.related(tx, "commercial_partner_id").await?;
    if !account.is_empty() && account.int()? != selected.int()? {
        pipeline::assert_company(tx, &account, company).await?;
        if account.cell(tx, "crm_is_business").await? == Cell::Bool(true)
            || account.cell(tx, "is_company").await? == Cell::Bool(true)
        {
            return Ok((account, Some(selected.int()?)));
        }
    }
    Ok((selected, None))
}

#[methods(QualifyWizard, addon = "crm")]
impl QualifyWizard {
    #[method]
    #[rpc(effect = write)]
    async fn action_convert(
        this: &RecordSet<QualifyWizard>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wiz = this.ensure_one()?.as_records();
        wiz.check_access(tx, Operation::Write).await?;
        let env = lifecycle::clean_env(this.env());
        let lead = wiz.related(tx, "lead_id").await?.with_env(env.clone());
        tx.savepoint(async |tx| {
            lifecycle::locked(tx, &lead).await?;
            if lead.cell(tx, "kind").await? == Cell::Str("opportunity".into()) {
                return Ok(form_action(
                    "crm.lead",
                    lead.int()?,
                    "Opportunity",
                    "current",
                ));
            }
            if lead.cell(tx, "state").await? != Cell::Str("open".into()) {
                return Err(Error::user("Reopen the lead before qualifying it."));
            }
            let company = lead.cell(tx, "company_id").await?;
            let Cell::Id(company_id) = company else {
                return Err(Error::invariant("The lead needs a company."));
            };
            let contacts = env.records("res.partner")?;
            let choice = this.get(tx, Self::CHOICE).await?;
            let person = RawVals::new()
                .set_cell("company_id", company.clone())
                .set_cell("email", lead.cell(tx, "email").await?)
                .set_cell("phone", lead.cell(tx, "phone").await?);
            let (account, contact_role) = match choice.as_deref() {
                Some("existing") => {
                    let selected = wiz.related(tx, "partner_id").await?;
                    existing_customer(tx, selected, company_id).await?
                }
                Some("person") => {
                    let name = nonblank(wiz.cell(tx, "contact_name").await?, "contact name")?;
                    let individual = contacts
                        .create(tx, vec![person.set_cell("name", name)])
                        .await?;
                    (individual, None)
                }
                Some("business") => {
                    let company_name =
                        nonblank(wiz.cell(tx, "company_name").await?, "company name")?;
                    let contact = wiz.cell(tx, "contact_name").await?;
                    let header = if contact.is_null() {
                        person.clone()
                    } else {
                        RawVals::new().set_cell("company_id", company)
                    };
                    let account = contacts
                        .create(
                            tx,
                            vec![
                                header
                                    .set_cell("name", company_name)
                                    .set_cell("crm_is_business", Cell::Bool(true)),
                            ],
                        )
                        .await?;
                    let contact_role = if contact.is_null() {
                        None
                    } else {
                        let person = contacts
                            .create(
                                tx,
                                vec![
                                    person
                                        .set_cell("name", nonblank(contact, "contact name")?)
                                        .set_cell("parent_id", Cell::Id(account.int()?)),
                                ],
                            )
                            .await?;
                        Some(person.int()?)
                    };
                    (account, contact_role)
                }
                _ => return Err(Error::validation("Choose how to link a customer.")),
            };
            lead.write_base(
                tx,
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(account.int()?))
                    .set_cell("kind", Cell::Str("opportunity".into())),
            )
            .await?;
            lifecycle::validate_lead(tx, &lead).await?;
            if let Some(person) = contact_role {
                let roles = env.records("crm.lead.contact")?;
                let assigned = Domain::field("lead_id")?.eq(lead.int()?)
                    & Domain::field("partner_id")?.eq(person);
                if roles.search_count(tx, &assigned, Some(1)).await? == 0 {
                    roles
                        .create(
                            tx,
                            vec![
                                RawVals::new()
                                    .set_cell("lead_id", Cell::Id(lead.int()?))
                                    .set_cell("partner_id", Cell::Id(person))
                                    .set_cell("role", Cell::Str("other".into())),
                            ],
                        )
                        .await?;
                }
            }
            Ok(form_action(
                "crm.lead",
                lead.int()?,
                "Opportunity",
                "current",
            ))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matching_is_exact_after_normalization_and_skips_short_phones() {
        assert_eq!(
            email_key(Some(" Ada@EXAMPLE.COM ")).as_deref(),
            Some("ada@example.com")
        );
        assert_eq!(
            phone_key(Some("00 62-812 345 678")),
            phone_key(Some("+62 (812) 345-678"))
        );
        assert!(phone_key(Some("123")).is_none());
        assert!(email_key(Some(" ")).is_none());
    }
}
