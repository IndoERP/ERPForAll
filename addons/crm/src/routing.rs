// SPDX-License-Identifier: MIT OR Apache-2.0
//! Team assignment and explainable scoring share a rule's source and region criteria.
use crate::CrmLead;
use erp_sdk::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
mod membership;
pub(crate) use membership::apply_member;

#[model(name = "crm.rule", description = "Lead Assignment and Scoring Rule", order = "sequence, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [("_weight", "CHECK(points BETWEEN -100 AND 100)", "Rule points must be between -100 and 100."),
                   ("_cursor", "CHECK(cursor_position >= 0)", "Routing cursor must be non-negative.")])]
pub struct CrmRule {
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(integer, default = 10)]
    pub sequence: i32,
    #[field(many2one, comodel = "res.company", required = true)]
    pub company_id: Option<RecId>,
    #[field(char, string = "Match Lead Source")]
    pub source: Option<Arc<str>>,
    #[field(char, string = "Match Region")]
    pub region: Option<Arc<str>>,
    #[field(many2one, comodel = "crm.team", string = "Assign to Team")]
    pub team_id: Option<RecId>,
    #[field(integer, string = "Score Points")]
    pub points: i32,
    #[field(integer, readonly = true, copy = false)]
    pub cursor_position: i32,
}

fn matches(criterion: Option<&str>, actual: Option<&str>) -> bool {
    criterion.is_none_or(|term| {
        term.trim().is_empty() || actual.is_some_and(|v| v.trim().eq_ignore_ascii_case(term.trim()))
    })
}

erp_sdk::model_ref!(pub TeamMember = "crm.team.member" { user_id: Option<RecId> });

struct Rule {
    id: i64,
    name: Arc<str>,
    source: Option<Arc<str>>,
    region: Option<Arc<str>>,
    company: i64,
    team: Option<RecId>,
    points: i32,
}

impl Rule {
    fn applies(&self, company: i64, source: Option<&str>, region: Option<&str>) -> bool {
        company == self.company
            && matches(self.source.as_deref(), source)
            && matches(self.region.as_deref(), region)
    }
}

async fn rules(
    env: &Env,
    tx: &mut Transaction,
    companies: Vec<i64>,
    criteria: Domain,
) -> Result<Vec<Rule>> {
    let domain =
        Domain::field("company_id")?.in_(companies) & Domain::field("active")?.eq(true) & criteria;
    let records = env.records("crm.rule")?.search(tx, &domain).await?;
    let set = RecordSet::<CrmRule>::try_from(records)?;
    let names = set.mapped(tx, CrmRule::NAME).await?;
    let sources = set.mapped(tx, CrmRule::SOURCE).await?;
    let regions = set.mapped(tx, CrmRule::REGION).await?;
    let points = set.mapped(tx, CrmRule::POINTS).await?;
    let teams = set.mapped(tx, CrmRule::TEAM_ID).await?;
    let companies = set.mapped(tx, CrmRule::COMPANY_ID).await?;
    set.as_records()
        .ids()
        .into_iter()
        .enumerate()
        .map(|(i, id)| {
            Ok(Rule {
                id,
                name: names[i]
                    .clone()
                    .ok_or_else(|| Error::invariant("A CRM rule has no name."))?,
                source: sources[i].clone(),
                region: regions[i].clone(),
                points: points[i],
                team: teams[i],
                company: companies[i]
                    .ok_or_else(|| Error::invariant("A CRM rule has no company."))?
                    .get(),
            })
        })
        .collect()
}

#[methods(CrmRule, addon = "crm")]
impl CrmRule {
    #[method(override)]
    async fn create(
        this: &RecordSet<CrmRule>,
        tx: &mut Transaction,
        mut vals: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if this
            .env()
            .context()
            .get("default_cursor_position")
            .is_some()
            || vals.iter().any(|v| v.contains("cursor_position"))
        {
            return Err(Error::validation(
                "Lead assignment manages the round-robin cursor.",
            ));
        }
        for row in &mut vals {
            row.insert("cursor_position".into(), Cell::Int(0));
        }
        let rules = sup.call(tx, this.as_records(), (vals,)).await?;
        invalidate_scores(tx).await?;
        Ok(rules)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<CrmRule>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("cursor_position") {
            return Err(Error::validation(
                "Lead assignment manages the round-robin cursor.",
            ));
        }
        sup.call(tx, this.as_records(), (vals,)).await?;
        invalidate_scores(tx).await
    }

    #[method(override)]
    async fn unlink(this: &RecordSet<CrmRule>, tx: &mut Transaction, sup: Super) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        invalidate_scores(tx).await
    }

    #[constrains("company_id", "team_id")]
    async fn _check_team(this: &RecordSet<CrmRule>, tx: &mut Transaction) -> Result<()> {
        for rule in this.as_records().iter() {
            let team = rule.related(tx, "team_id").await?;
            if !team.is_empty()
                && let Cell::Id(company) = team.cell(tx, "company_id").await?
                && rule.cell(tx, "company_id").await? != Cell::Id(company)
            {
                return Err(Error::validation(
                    "Choose a sales team in the rule's company.",
                ));
            }
        }
        Ok(())
    }
}

async fn invalidate_scores(tx: &mut Transaction) -> Result<()> {
    tx.invalidate_model("crm.lead", Some(&["score", "score_reasons"]))
        .await
}

fn text(vals: &RawVals, key: &str) -> Option<Arc<str>> {
    match vals.get(key) {
        Some(Cell::Str(value)) => Some(value.clone()),
        _ => None,
    }
}

/// Account records are private. Only test the named internal user's membership in an
/// already visible company, including historical owners whose login is now archived.
pub(crate) async fn user_in_company(
    env: &Env,
    tx: &mut Transaction,
    user: i64,
    company: i64,
) -> Result<bool> {
    env.records("res.company")?
        .browse([company])?
        .check_access(tx, erp_sdk::Operation::Read)
        .await?;
    let eligible = Domain::field("id")?.eq(user)
        & Domain::field("share")?.eq(false)
        & Domain::condition("company_ids", Operator::In, vec![company])?;
    Ok(env
        .sudo(true)
        .with_context([("active_test", Value::Bool(false))])
        .records("res.users")?
        .search_count(tx, &eligible, Some(1))
        .await?
        != 0)
}

/// Resolve the same import, action and user defaults as ORM create before routing and FX run.
pub(crate) async fn apply_intake_defaults(
    leads: &Records,
    tx: &mut Transaction,
    rows: &mut [RawVals],
) -> Result<()> {
    let keys = [
        "source",
        "region",
        "deal_currency_id",
        "deal_amount",
        "expected_revenue",
        "salesperson_member_id",
    ];
    let mut by_company = BTreeMap::new();
    for vals in rows {
        // The form default for deal currency follows the company of this row, including
        // mixed-company imports. Keep every default lookup inside the caller's active scope.
        let company = match vals.get("company_id") {
            Some(Cell::Id(id)) => Some(*id),
            _ => match leads.env().context().get("default_company_id") {
                Some(Value::Int(id)) => Some(id),
                _ => leads.env().company_id()?,
            },
        };
        let defaults = match by_company.entry(company) {
            std::collections::btree_map::Entry::Occupied(found) => found.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let scoped = if let Some(id) = company {
                    let company = leads.env().records("res.company")?.browse([id])?;
                    company.check_access(tx, erp_sdk::Operation::Read).await?;
                    leads.with_env(
                        leads
                            .env()
                            .with_company(id)?
                            .with_context([("default_company_id", Value::Int(id))]),
                    )
                } else {
                    leads.clone()
                };
                entry.insert(scoped.vals_from_values(&scoped.default_get(tx, &keys).await?)?)
            }
        };
        let entered_amount = vals.contains("deal_amount") || vals.contains("expected_revenue");
        let entered_owner = vals.contains("user_id");
        for key in keys {
            if !vals.contains(key)
                && !(entered_amount && matches!(key, "deal_amount" | "expected_revenue"))
                && !(entered_owner && key == "salesperson_member_id")
                && let Some(value) = defaults.get(key)
            {
                vals.insert(key.into(), value.clone());
            }
        }
    }
    Ok(())
}

pub(crate) async fn route_new(env: &Env, tx: &mut Transaction, vals: &mut RawVals) -> Result<()> {
    if matches!(vals.get("user_id"), Some(Cell::Id(_)))
        || text(vals, "kind").as_deref() == Some("opportunity")
    {
        return Ok(());
    }
    let Some(Cell::Id(company)) = vals.get("company_id") else {
        return Err(Error::validation("Lead routing requires a company."));
    };
    let company = *company;
    let source = text(vals, "source");
    let region = text(vals, "region");
    let requested = vals.get("team_id").cloned();
    let rules = rules(
        env,
        tx,
        vec![company],
        Domain::field("team_id.active")?.eq(true),
    )
    .await?;
    for rule in rules {
        let Some(team) = rule.team else { continue };
        if !rule.applies(company, source.as_deref(), region.as_deref())
            || matches!(requested,Some(Cell::Id(t)) if t != team.get())
        {
            continue;
        }
        vals.insert("team_id".into(), Cell::Id(team.get()));
        let eligible =
            Domain::field("crm_team_id")?.eq(team.get()) & Domain::field("active")?.eq(true);
        let members = RecordSet::<TeamMember>::try_from(
            env.records("crm.team.member")?
                .search(tx, &eligible)
                .await?,
        )?;
        let candidates: Vec<_> = members
            .mapped(tx, TeamMember::USER_ID)
            .await?
            .into_iter()
            .flatten()
            .map(RecId::get)
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        // Filtering private user accounts must select the same salespeople for a
        // manager and for a sales rep who can read only their own account.
        let scoped = Domain::field("id")?.in_(candidates)
            & Domain::field("active")?.eq(true)
            & Domain::field("share")?.eq(false)
            & Domain::condition("company_ids", Operator::In, vec![company])?;
        let mut users = env
            .sudo(true)
            .records("res.users")?
            .search(tx, &scoped)
            .await?
            .ids();
        if users.is_empty() {
            return Ok(());
        }
        users.sort_unstable();
        // Advance atomically in the same transaction as the insert. Failed creates roll back the cursor.
        let count = i32::try_from(users.len())
            .map_err(|_| Error::validation("Too many sales team members."))?;
        tx.flush().await?;
        let rows = tx.query("UPDATE crm_rule SET cursor_position = (COALESCE(cursor_position,0) % $2 + 1) % $2 WHERE id = $1 AND active RETURNING (cursor_position + $2 - 1) % $2", &[&rule.id,&count]).await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::validation("Routing rule changed. Retry the assignment."))?;
        let cursor: i32 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("Invalid lead assignment cursor: {e}")))?;
        tx.invalidate_records(
            &env.records("crm.rule")?.browse([rule.id])?,
            Some(&["cursor_position"]),
        )
        .await?;
        let user = users
            .get(cursor as usize)
            .ok_or_else(|| Error::invariant("Invalid lead assignment cursor."))?;
        vals.insert("user_id".into(), Cell::Id(*user));
        apply_member(env, tx, None, vals).await?;
        return Ok(());
    }
    Ok(())
}

#[extend(CrmLead, addon = "crm")]
pub struct LeadScore {
    #[field(integer, compute = "_compute_crm_score", string = "Lead Score")]
    pub score: i32,
    #[field(text, compute = "_compute_crm_score", string = "Score Breakdown")]
    pub score_reasons: Option<Arc<str>>,
}

fn evaluate<'a>(matched: impl Iterator<Item = &'a Rule>) -> (i32, String) {
    let mut total = 0_i64;
    let mut reasons = Vec::new();
    for rule in matched.filter(|rule| rule.points != 0) {
        total += i64::from(rule.points);
        reasons.push(format!("{} ({:+})", rule.name, rule.points));
    }
    (total.clamp(0, 100) as i32, reasons.join("\n"))
}

#[methods(CrmLead, addon = "crm")]
impl LeadScore {
    #[compute(depends = ["source", "region", "company_id"], depends_context = ["uid","allowed_company_ids"])]
    async fn _compute_crm_score(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        out: &mut Assign<'_, CrmLead>,
    ) -> Result<()> {
        let companies = this.mapped(tx, CrmLead::COMPANY_ID).await?;
        let sources = this.mapped(tx, CrmLead::SOURCE).await?;
        let regions = this.mapped(tx, CrmLead::REGION).await?;
        let ids: BTreeSet<_> = companies.iter().flatten().map(|id| id.get()).collect();
        let mut by_company = BTreeMap::<i64, Vec<Rule>>::new();
        for rule in rules(
            this.env(),
            tx,
            ids.into_iter().collect(),
            Domain::constant(true),
        )
        .await?
        {
            by_company.entry(rule.company).or_default().push(rule);
        }
        for (index, company) in companies.iter().enumerate() {
            let company = company.map(RecId::get);
            let matched = company
                .and_then(|id| by_company.get(&id))
                .into_iter()
                .flatten()
                .filter(|rule| {
                    rule.applies(
                        rule.company,
                        sources[index].as_deref(),
                        regions[index].as_deref(),
                    )
                });
            let (score, reasons) = evaluate(matched);
            out.set(index, LeadScore::SCORE, score)?;
            out.set(
                index,
                LeadScore::SCORE_REASONS,
                Some(Arc::<str>::from(reasons)),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoring_explains_only_matching_rules_and_caps_the_total() {
        let rule = |name: &str, source: Option<&str>, points| Rule {
            id: 1,
            name: name.into(),
            source: source.map(Into::into),
            region: None,
            company: 1,
            team: None,
            points,
        };
        let rules = [
            rule("Inbound", Some("Web"), 75),
            rule("Regional", None, 40),
            rule("Referral", Some("Referral"), 5),
        ];
        let score =
            |company, source| evaluate(rules.iter().filter(|r| r.applies(company, source, None)));
        assert_eq!(
            score(1, Some(" web ")),
            (100, "Inbound (+75)\nRegional (+40)".into())
        );
        assert_eq!(score(2, None), (0, String::new()));
        assert_eq!(score(1, None), (40, "Regional (+40)".into()));
    }
}
