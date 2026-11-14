// SPDX-License-Identifier: MIT OR Apache-2.0
//! Calendar ranges use PostgreSQL's existing timezone semantics, in the request snapshot.
use crate::dashboard_settings::Period;
use erp_sdk::prelude::*;
pub(super) struct Ranges {
    pub current: (String, String),
    pub previous: (String, String),
}
pub(super) async fn ranges(
    tx: &mut Transaction,
    period: &Period,
    timezone: &str,
    datetime: bool,
    max_days: u32,
    today: Option<Date>,
) -> Result<Ranges> {
    if tx
        .query(
            "SELECT 1 FROM pg_timezone_names WHERE name=$1",
            &[&timezone],
        )
        .await?
        .is_empty()
    {
        return Err(Error::validation(format!(
            "Dashboard timezone '{timezone}' is not recognized."
        )));
    }
    let days = i32::try_from(period.days.unwrap_or(1))
        .map_err(|_| Error::validation("Dashboard period days overflow."))?; // calendar periods do not use this parameter
    let rows = tx.query(
        "WITH d AS (SELECT COALESCE($4::date, timezone($1, CURRENT_TIMESTAMP)::date) AS today), \
         p AS (SELECT today + 1 AS finish, CASE $2::text WHEN 'rolling' THEN today - ($3::int - 1) WHEN 'month' THEN date_trunc('month', today)::date WHEN 'year' THEN date_trunc('year', today)::date END AS start FROM d), \
         q AS (SELECT *, CASE $2::text WHEN 'rolling' THEN start - $3::int WHEN 'month' THEN (start - interval '1 month')::date WHEN 'year' THEN (start - interval '1 year')::date END AS previous FROM p) \
         SELECT CASE WHEN $5::boolean THEN timezone('UTC', timezone($1, boundary::timestamp))::text ELSE boundary::text END, finish - start \
         FROM q CROSS JOIN LATERAL unnest(ARRAY[start, finish, previous, LEAST(start, previous + (finish - start))]) WITH ORDINALITY AS b(boundary, ordinal) ORDER BY ordinal",
        &[&timezone, &period.kind, &days, &today, &datetime],
    ).await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::invariant("Dashboard period query returned no row."))?;
    let span: i32 = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
    if span < 1
        || u32::try_from(span).map_err(|_| Error::validation("Dashboard period span overflow."))?
            > max_days
    {
        return Err(Error::validation(
            "Dashboard period exceeds web.dashboard_max_period_days.",
        ));
    }
    if rows.len() != 4 {
        return Err(Error::invariant(
            "Dashboard period query requires four boundaries.",
        ));
    }
    let values = rows
        .iter()
        .map(|row| {
            row.try_get::<_, String>(0)
                .map_err(|e| Error::db(e.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Ranges {
        current: (values[0].clone(), values[1].clone()),
        previous: (values[2].clone(), values[3].clone()),
    })
}
