// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;
use erp_addon_reporting::runtime;
use erp_fields::{Date, Decimal};
use erp_orm::Env;
use serde_json::{Value as Json, json};

async fn forecast(
    env: &Env,
    tx: &mut Transaction,
    company: i64,
    month: Date,
    metric: &str,
    measure: &str,
) -> Decimal {
    let id = xmlid(tx, "crm", "forecast_report").await;
    let (y, m, _) = month.ymd();
    let following = if m == 12 {
        Date::from_ymd(y + 1, 1, 1)
    } else {
        Date::from_ymd(y, m + 1, 1)
    }
    .unwrap();
    let end = Date::from_days(following.days() - 1);
    let options = runtime::report_options(
        env,
        tx,
        id,
        &json!({
            "report":id, "companies":[company],
            "date":{"preset":"custom","from":month.to_string(),"to":end.to_string()}
        }),
    )
    .await
    .unwrap()["options"]
        .clone();
    let column = options["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c["label"] == measure)
        .unwrap();
    let report = runtime::report_lines(env, tx, &options).await.unwrap();
    let row = report["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["name"] == metric)
        .unwrap();
    let cell = row["cells"].as_array().unwrap().get(column).unwrap();
    match cell.get("value").expect("every report cell has a value") {
        Json::String(n) => n.parse().unwrap(),
        Json::Null => Decimal::ZERO,
        value => panic!("invalid numeric report value {value}"),
    }
}

#[test]
fn monthly_forecast_separates_close_dates_quota_and_frozen_fx() {
    run(async {
        let (_db, system, _) = cloned_sale_system().await;
        let installed = Box::pin(system.button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("crm").unwrap()],
        ))
        .await
        .unwrap();
        let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        admin(&mut tx).await;
        let seller = user(&mut tx, "forecast-owner", "group_sale_salesman").await;
        let team = team(&mut tx, "Forecast team", seller).await;
        let env = tx.env().with_company(world.company).unwrap();
        let today = env.context_today().unwrap();
        let (y, m, _) = today.ymd();
        let month = Date::from_ymd(y, m, 1).unwrap();
        let next = if m == 12 {
            Date::from_ymd(y + 1, 1, 1)
        } else {
            Date::from_ymd(y, m + 1, 1)
        }
        .unwrap();
        let yesterday = Date::from_days(today.days() - 1);
        let (home, foreign) = prices::foreign_rates(&mut tx, &world, yesterday, "0.01").await;
        let quotas = env.records("crm.quota").unwrap();
        let periods = [month, next]
            .map(|start| {
                RawVals::new()
                    .set_cell("user_id", Cell::Id(seller))
                    .set_cell("team_id", Cell::Id(team))
                    .set_cell("company_id", Cell::Id(world.company))
                    .set_cell("month", Cell::Date(start))
                    .set_cell("amount", Cell::Decimal(1_500.into()))
            })
            .to_vec();
        let quota_rows = quotas.create(&mut tx, periods).await.unwrap();
        let current_quota = quota_rows.iter().next().unwrap();
        assert_eq!(
            current_quota
                .cell(&mut tx, "achieved_amount")
                .await
                .unwrap(),
            Cell::Decimal(Decimal::ZERO)
        );
        let lead = env
            .records("crm.lead")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("kind", Cell::Str("opportunity".into()))
                        .set_cell("name", Cell::Str("Export deal".into()))
                        .set_cell("user_id", Cell::Id(seller))
                        .set_cell("team_id", Cell::Id(team))
                        .set_cell("partner_id", Cell::Id(world.customer))
                        .set_cell("date_deadline", Cell::Date(next))
                        .set_cell("deal_currency_id", Cell::Id(foreign))
                        .set_cell("deal_amount", Cell::Decimal(prices::dec("9.995"))),
                ],
            )
            .await
            .unwrap();
        // Quantize the original currency before applying the 100:1 rate. This keeps
        // the open forecast and the eventual Won amount based on the same principal.
        assert_eq!(
            lead.cell(&mut tx, "deal_amount").await.unwrap(),
            Cell::Decimal(10.into())
        );
        assert_eq!(
            lead.cell(&mut tx, "expected_revenue").await.unwrap(),
            Cell::Decimal(1_000.into())
        );
        assert_eq!(
            forecast(
                &env,
                &mut tx,
                world.company,
                next,
                "Open Forecast",
                "weighted"
            )
            .await,
            Decimal::from(100)
        );
        let _: () = lead
            .call(&mut tx, "action_win", ("Signed contract".to_string(),))
            .await
            .unwrap();
        assert_eq!(
            lead.cell(&mut tx, "closed_date").await.unwrap(),
            Cell::Date(today)
        );
        assert_eq!(
            lead.cell(&mut tx, "won_currency_id").await.unwrap(),
            Cell::Id(home)
        );
        assert_eq!(
            lead.cell(&mut tx, "won_amount").await.unwrap(),
            Cell::Decimal(1_000.into())
        );
        env.records("res.currency.rate")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Date(today))
                        .set_cell("rate", Cell::Decimal(2.into()))
                        .set_cell("company_id", Cell::Id(world.company))
                        .set_cell("currency_id", Cell::Id(foreign)),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            lead.cell(&mut tx, "won_amount").await.unwrap(),
            Cell::Decimal(1_000.into())
        );
        assert_eq!(
            forecast(
                &env,
                &mut tx,
                world.company,
                next,
                "Remaining to Target",
                "amount"
            )
            .await,
            Decimal::from(1_500)
        );
        assert_eq!(
            forecast(&env, &mut tx, world.company, next, "Closed Won", "amount").await,
            Decimal::ZERO
        );
        assert_eq!(
            forecast(&env, &mut tx, world.company, month, "Closed Won", "amount").await,
            Decimal::from(1_000)
        );
        assert_eq!(
            forecast(
                &env,
                &mut tx,
                world.company,
                month,
                "Remaining to Target",
                "amount"
            )
            .await,
            Decimal::from(500)
        );
        assert_eq!(
            current_quota
                .cell(&mut tx, "remaining_amount")
                .await
                .unwrap(),
            Cell::Decimal(500.into())
        );
        let action: Value = current_quota
            .call(&mut tx, "action_view_credited_deals", ())
            .await
            .unwrap();
        let Value::Json(action) = action else {
            panic!("expected a window action")
        };
        let credited = erp_sdk::Domain::from_json(&action["domain"]).unwrap();
        assert_eq!(
            env.records("crm.lead")
                .unwrap()
                .search(&mut tx, &credited)
                .await
                .unwrap()
                .ids(),
            lead.ids()
        );
        // The winning period follows the date of the decision even when the expected close was next month.
        let _: () = lead.call(&mut tx, "action_reopen", ()).await.unwrap();
        assert_eq!(
            current_quota
                .cell(&mut tx, "remaining_amount")
                .await
                .unwrap(),
            Cell::Decimal(1_500.into())
        );
        assert_eq!(
            forecast(&env, &mut tx, world.company, month, "Closed Won", "amount").await,
            Decimal::ZERO
        );
        assert_eq!(
            forecast(
                &env,
                &mut tx,
                world.company,
                next,
                "Open Forecast",
                "amount"
            )
            .await,
            Decimal::from(1_000)
        );
        tx.rollback().await.unwrap();
    });
}
