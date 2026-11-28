// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native picker selects an existing team membership; integrations may send user_id.
use erp_sdk::Operation;
use erp_sdk::prelude::*;

const MEMBER: &str = "salesperson_member_id";

async fn id(
    tx: &mut Transaction,
    vals: &RawVals,
    record: Option<&Records>,
    field: &str,
) -> Result<Option<i64>> {
    let cell = match (vals.get(field), record) {
        (Some(value), _) => value.clone(),
        (None, Some(row)) => row.cell(tx, field).await?,
        (None, None) => Cell::Null,
    };
    match cell {
        Cell::Id(id) => Ok(Some(id)),
        Cell::Null => Ok(None),
        _ => Err(Error::validation(format!(
            "Select a valid value for {field}."
        ))),
    }
}

pub(crate) async fn apply_member(
    env: &Env,
    tx: &mut Transaction,
    record: Option<&Records>,
    vals: &mut RawVals,
) -> Result<()> {
    if ![MEMBER, "user_id", "team_id"]
        .iter()
        .any(|key| vals.contains(key))
    {
        return Ok(());
    }
    let members = env.records("crm.team.member")?;
    if let Some(Cell::Id(selected)) = vals.get(MEMBER) {
        let member = members.browse([*selected])?;
        member.check_access(tx, Operation::Read).await?;
        let team = member.related(tx, "crm_team_id").await?;
        let company = id(tx, vals, record, "company_id")
            .await?
            .or(env.company_id()?)
            .ok_or_else(|| Error::validation("Select a company before assigning a salesperson."))?;
        crate::pipeline::assert_company(tx, &team, company).await?;
        if member.cell(tx, "active").await? != Cell::Bool(true)
            || team.cell(tx, "active").await? != Cell::Bool(true)
        {
            return Err(Error::validation(
                "Choose a member of an active sales team.",
            ));
        }
        let user = member.cell(tx, "user_id").await?;
        let Cell::Id(user_id) = user else {
            return Err(Error::invariant("Sales team membership has no user."));
        };
        if !super::user_in_company(env, tx, user_id, company).await?
            || env
                .sudo(true)
                .records("res.users")?
                .browse([user_id])?
                .cell(tx, "active")
                .await?
                != Cell::Bool(true)
        {
            return Err(Error::validation(
                "Choose an active salesperson in this company.",
            ));
        }
        let team = Cell::Id(team.int()?);
        if vals
            .get("user_id")
            .is_some_and(|value| !value.is_null() && value != &user)
            || vals
                .get("team_id")
                .is_some_and(|value| !value.is_null() && value != &team)
        {
            return Err(Error::validation(
                "The selected membership must match the salesperson and team.",
            ));
        }
        vals.insert("user_id".into(), user);
        vals.insert("team_id".into(), team);
        return Ok(());
    }
    if vals.contains(MEMBER) && !vals.contains("user_id") {
        if !matches!(vals.get(MEMBER), Some(Cell::Null)) {
            return Err(Error::validation("Select a valid sales team member."));
        }
        vals.insert("user_id".into(), Cell::Null);
        return Ok(());
    }
    let user = id(tx, vals, record, "user_id").await?;
    let team = id(tx, vals, record, "team_id").await?;
    let selected = if let (Some(user), Some(team)) = (user, team) {
        let matching = Domain::field("user_id")?.eq(user)
            & Domain::field("crm_team_id")?.eq(team)
            & Domain::field("active")?.eq(true);
        crate::pipeline::first(tx, &members, &matching).await?
    } else {
        None
    };
    vals.insert(MEMBER.into(), selected.map_or(Cell::Null, Cell::Id));
    Ok(())
}
