// SPDX-License-Identifier: MIT OR Apache-2.0
//! A prospect keeps the same record, activities and conversation as it becomes a deal.
use erp_sdk::prelude::*;

erp_sdk::model_ref!(pub MailThread = "mail.thread");
erp_sdk::model_ref!(pub Activity = "mail.activity" { active: bool, date_deadline: Option<Date> });
erp_sdk::model_ref!(pub MailActivityMixin = "mail.activity.mixin" { activity_ids: X2many<Activity> });

#[model(name = "crm.lead", description = "Lead / Opportunity", inherit = [MailThread, MailActivityMixin],
    order = "id DESC", company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [("_amount_check", "CHECK(expected_revenue >= 0)", "Expected revenue must be positive or zero."),
                   ("_probability_check", "CHECK(probability >= 0 AND probability <= 100)", "Probability must be between 0 and 100.")])]
pub struct CrmLead {
    #[field(char, required = true, size = 256, index = "trigram", string = "Deal")]
    pub name: Option<Arc<str>>,
    #[field(selection, selection = [("lead","Lead"),("opportunity","Opportunity")], default = "lead", required = true, index = true, tracking = true)]
    pub kind: Option<Arc<str>>,
    #[field(boolean, readonly = true, string = "Originated as Lead")]
    pub originated_as_lead: bool,
    #[field(selection, selection = [("open","Open"),("won","Won"),("lost","Lost")], default = "open", required = true, readonly = true, index = true, tracking = true)]
    pub state: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        ondelete = "restrict",
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "crm.team",
        index = true,
        string = "Sales Team",
        ondelete = "restrict",
        tracking = true
    )]
    pub team_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.users",
        index = true,
        string = "Salesperson",
        tracking = true
    )]
    pub user_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "crm.team.member",
        ondelete = "set null",
        copy = false,
        string = "Assign Team Member"
    )]
    pub salesperson_member_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "crm.pipeline",
        index = true,
        required = true,
        ondelete = "restrict",
        string = "Pipeline",
        tracking = true
    )]
    pub pipeline_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "crm.stage",
        index = true,
        required = true,
        ondelete = "restrict",
        string = "Stage",
        tracking = true
    )]
    pub stage_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.partner",
        index = true,
        ondelete = "restrict",
        string = "Customer",
        tracking = true
    )]
    pub partner_id: Option<RecId>,
    #[field(char, string = "Company Name", size = 256)]
    pub company_name: Option<Arc<str>>,
    #[field(char, string = "Contact Name", size = 256)]
    pub contact_name: Option<Arc<str>>,
    #[field(char, size = 320, string = "Email")]
    pub email: Option<Arc<str>>,
    #[field(char, size = 64, string = "Phone")]
    pub phone: Option<Arc<str>>,
    #[field(char, size = 128, string = "Source", index = true)]
    pub source: Option<Arc<str>>,
    #[field(char, size = 128, string = "Region", index = true)]
    pub region: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "crm.tag",
        relation = "crm_lead_tag_rel",
        column1 = "lead_id",
        column2 = "tag_id",
        string = "Tags"
    )]
    pub tag_ids: Vec<RecId>,
    #[field(
        one2many,
        comodel = "crm.lead.contact",
        inverse_name = "lead_id",
        string = "Contact Roles"
    )]
    pub contact_role_ids: Vec<RecId>,
    #[field(
        one2many,
        comodel = "crm.stage.history",
        inverse_name = "lead_id",
        readonly = true,
        string = "Stage History"
    )]
    pub history_ids: Vec<RecId>,
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Company Currency"
    )]
    pub currency_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.currency",
        string = "Deal Currency",
        ondelete = "restrict"
    )]
    pub deal_currency_id: Option<RecId>,
    #[field(monetary, currency_field = "deal_currency_id", string = "Deal Amount")]
    pub deal_amount: Decimal,
    #[field(char, readonly = true, string = "Exchange Rate at Win")]
    pub won_rate: Option<Arc<str>>,
    #[field(
        monetary,
        currency_field = "currency_id",
        tracking = true,
        string = "Expected Revenue"
    )]
    pub expected_revenue: Decimal,
    #[field(integer, string = "Probability (%)", tracking = true)]
    pub probability: i32,
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_weighted_revenue",
        store = true,
        string = "Weighted Revenue"
    )]
    pub weighted_revenue: Decimal,
    #[field(selection, selection = [("pipeline","Pipeline"),("best_case","Best Case"),("commit","Commit"),("omitted","Omitted"),("closed_won","Closed Won")], default = "pipeline", required = true, tracking = true, index = true, string = "Forecast Category")]
    pub forecast_category: Option<Arc<str>>,
    #[field(date, index = true, string = "Expected Close", tracking = true)]
    pub date_deadline: Option<Date>,
    #[field(char, size = 512, string = "Next Step", tracking = true)]
    pub next_step: Option<Arc<str>>,
    #[field(
        date,
        compute = "_compute_next_activity_date",
        store = true,
        index = true,
        string = "Next Activity"
    )]
    pub next_activity_date: Option<Date>,
    #[field(datetime, readonly = true, string = "Entered Stage")]
    pub stage_since: Option<DateTime>,
    #[field(date, readonly = true, index = true, string = "Closed On")]
    pub closed_date: Option<Date>,
    #[field(text, readonly = true, string = "Closing Reason", tracking = true)]
    pub close_reason: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.currency",
        string = "Won Currency",
        readonly = true,
        ondelete = "restrict"
    )]
    pub won_currency_id: Option<RecId>,
    #[field(
        monetary,
        currency_field = "won_currency_id",
        readonly = true,
        string = "Won Amount"
    )]
    pub won_amount: Decimal,
    #[field(
        many2one,
        comodel = "res.users",
        readonly = true,
        index = true,
        string = "Salesperson at Win",
        ondelete = "restrict"
    )]
    pub won_user_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "crm.team",
        readonly = true,
        index = true,
        string = "Team at Win",
        ondelete = "restrict"
    )]
    pub won_team_id: Option<RecId>,
}

#[methods(CrmLead, addon = "crm")]
impl CrmLead {
    #[compute(depends = ["expected_revenue","probability","state"])]
    fn _compute_weighted_revenue(batch: &mut Batch<'_, CrmLead>) -> Result<()> {
        for i in batch.indices() {
            let amount = batch.get(i, Self::EXPECTED_REVENUE)?;
            let probability = batch.get(i, Self::PROBABILITY)?;
            let state = batch.get(i, Self::STATE)?;
            let weighted = if state.as_deref() == Some("open") {
                amount
                    .checked_mul(Decimal::from(probability) / Decimal::from(100))
                    .ok_or_else(|| {
                        Error::validation("Weighted revenue exceeds the supported range.")
                    })?
            } else {
                Decimal::ZERO
            };
            batch.set(i, Self::WEIGHTED_REVENUE, weighted)?;
        }
        Ok(())
    }
    #[compute(depends = ["activity_ids","activity_ids.active","activity_ids.date_deadline"])]
    fn _compute_next_activity_date(batch: &mut Batch<'_, CrmLead>) -> Result<()> {
        for i in batch.indices() {
            let mut nearest = None;
            for &id in batch.get(i, MailActivityMixin::ACTIVITY_IDS)?.ids() {
                if batch.value_of(Activity::ACTIVE, id)?
                    && let Some(day) = batch.value_of(Activity::DATE_DEADLINE, id)?
                {
                    nearest = Some(nearest.map_or(day, |previous: Date| previous.min(day)));
                }
            }
            batch.set(i, Self::NEXT_ACTIVITY_DATE, nearest)?;
        }
        Ok(())
    }
}
