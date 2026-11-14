// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::{dashboard_period, dashboard_settings};
/// Test-only period probe; supplies a fixed local date without exposing a public RPC clock override.
pub async fn test_dashboard_ranges(
    tx: &mut erp_sdk::Transaction,
    kind: &str,
    days: Option<u32>,
    timezone: &str,
    datetime: bool,
    today: erp_sdk::Date,
    maximum: u32,
) -> erp_sdk::Result<((String, String), (String, String))> {
    let period = dashboard_settings::Period {
        id: "test".to_owned(),
        label: "Test".to_owned(),
        kind: kind.to_owned(),
        days,
    };
    let result =
        dashboard_period::ranges(tx, &period, timezone, datetime, maximum, Some(today)).await?;
    Ok((result.current, result.previous))
}
