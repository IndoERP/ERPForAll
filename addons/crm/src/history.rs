// SPDX-License-Identifier: MIT OR Apache-2.0
//! One immutable visit per stage, closed when a deal leaves the stage or is closed.
use erp_sdk::prelude::*;

#[model(name = "crm.stage.history", description = "Time in Stage", rec_name = "stage_id", order = "entered_at DESC, id DESC",
company_access = direct(field = "company_id", branch = "exact", empty = "deny", parent = "lead_id"),
unique_indexes = [("_one_open", "(lead_id) WHERE exited_at IS NULL", "The deal already has an open stage visit.")])]
pub struct StageHistory {
    #[field(
        many2one,
        comodel = "crm.lead",
        required = true,
        index = true,
        ondelete = "cascade",
        string = "Opportunity"
    )]
    pub lead_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "crm.stage",
        required = true,
        ondelete = "restrict",
        string = "Stage"
    )]
    pub stage_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.company",
        related = "lead_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.users",
        related = "lead_id.user_id",
        store = true,
        string = "Salesperson"
    )]
    pub owner_id: Option<RecId>,
    #[field(datetime, required = true, string = "Entered At")]
    pub entered_at: Option<DateTime>,
    #[field(datetime, string = "Exited At")]
    pub exited_at: Option<DateTime>,
    #[field(float, digits = (16,6), string = "Days in Stage")]
    pub duration_days: Decimal,
    #[field(float, digits = (16,6), compute = "_compute_elapsed_days", string = "Days in Stage")]
    pub elapsed_days: Decimal,
    #[field(selection, selection = [("moved","Advanced"),("won","Won"),("lost","Lost")], string = "Outcome")]
    pub outcome: Option<Arc<str>>,
    #[field(char, string = "Closing Reason")]
    pub closing_reason: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.currency",
        ondelete = "restrict",
        string = "Closing Currency"
    )]
    pub closing_currency_id: Option<RecId>,
    #[field(
        monetary,
        currency_field = "closing_currency_id",
        string = "Closing Amount"
    )]
    pub closing_amount: Decimal,
    #[field(
        many2one,
        comodel = "res.users",
        ondelete = "restrict",
        string = "Salesperson at Close"
    )]
    pub closing_user_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "crm.team",
        ondelete = "restrict",
        string = "Team at Close"
    )]
    pub closing_team_id: Option<RecId>,
    #[field(char, string = "Closing Exchange Rate")]
    pub closing_rate: Option<Arc<str>>,
}

fn elapsed(entered: Option<DateTime>, exited: Option<DateTime>, now: DateTime) -> Decimal {
    let Some(start) = entered else {
        return Decimal::ZERO;
    };
    let end = exited.unwrap_or(now);
    let micros = (i128::from(end.micros()) - i128::from(start.micros())).max(0);
    (Decimal::from(micros) / Decimal::from(86_400_000_000_i64)).round_dp(6)
}

#[methods(StageHistory, addon = "crm")]
impl StageHistory {
    #[compute(depends = ["entered_at","exited_at"])]
    fn _compute_elapsed_days(batch: &mut Batch<'_, StageHistory>) -> Result<()> {
        let now = DateTime::now();
        for i in batch.indices() {
            let days = elapsed(
                batch.get(i, Self::ENTERED_AT)?,
                batch.get(i, Self::EXITED_AT)?,
                now,
            );
            batch.set(i, Self::ELAPSED_DAYS, days)?;
        }
        Ok(())
    }
}

/// Elevation only for immutable derived history; the caller already locked and authorized the parent.
pub(crate) async fn open(
    tx: &mut Transaction,
    lead: &Records,
    stage: i64,
    at: DateTime,
) -> Result<()> {
    crate::lifecycle::clean_env(lead.env())
        .sudo(true)
        .records("crm.stage.history")?
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("lead_id", Cell::Id(lead.int()?))
                    .set_cell("stage_id", Cell::Id(stage))
                    .set_cell("entered_at", Cell::DateTime(at)),
            ],
        )
        .await?;
    Ok(())
}

pub(crate) async fn close(
    tx: &mut Transaction,
    lead: &Records,
    at: DateTime,
    closing: Option<&RawVals>,
) -> Result<()> {
    let history = crate::lifecycle::clean_env(lead.env())
        .sudo(true)
        .records("crm.stage.history")?;
    let open = history
        .search(
            tx,
            &(Domain::condition("lead_id", Operator::Eq, lead.int()?)?
                & Domain::condition("exited_at", Operator::Eq, false)?),
        )
        .await?;
    open.ensure_one()?;
    let Cell::DateTime(entered) = open.cell(tx, "entered_at").await? else {
        return Err(Error::invariant("The open stage visit has no entry time."));
    };
    let days = elapsed(Some(entered), Some(at), at);
    let mut vals = RawVals::new()
        .set_cell("exited_at", Cell::DateTime(at))
        .set_cell("duration_days", Cell::Decimal(days))
        .set_cell("outcome", Cell::Str("moved".into()));
    if let Some(closing) = closing {
        for (src, dest) in [
            ("state", "outcome"),
            ("close_reason", "closing_reason"),
            ("won_currency_id", "closing_currency_id"),
            ("won_amount", "closing_amount"),
            ("won_user_id", "closing_user_id"),
            ("won_team_id", "closing_team_id"),
            ("won_rate", "closing_rate"),
        ] {
            if let Some(value) = closing.get(src) {
                vals.insert(dest.into(), value.clone());
            }
        }
    }
    open.write(tx, vals).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_stage_age_advances_until_the_visit_closes() {
        let day = 86_400_000_000_i64;
        let start = Some(DateTime::from_micros(day));
        let now = DateTime::from_micros(3 * day);
        assert_eq!(elapsed(start, None, now), Decimal::from(2));
        assert_eq!(
            elapsed(start, Some(DateTime::from_micros(2 * day)), now),
            Decimal::ONE
        );
        assert_eq!(elapsed(None, None, now), Decimal::ZERO);
        assert_eq!(
            elapsed(start, None, DateTime::from_micros(0)),
            Decimal::ZERO
        );
    }
}
