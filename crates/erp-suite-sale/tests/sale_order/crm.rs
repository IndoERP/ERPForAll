// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;

fn returned_form(action: &Value, model: &str) -> i64 {
    let Value::Map(values) = action else {
        panic!("expected a window")
    };
    assert_eq!(values["type"], Value::str("ir.actions.act_window"));
    assert_eq!(values["target"], Value::str("current"));
    assert_eq!(values["res_model"], Value::str(model));
    assert_eq!(
        values["views"],
        Value::list([Value::list([Value::False, Value::str("form")])])
    );
    let Value::Int(id) = values["res_id"] else {
        panic!("expected an existing record")
    };
    id
}

async fn review_count(tx: &mut Transaction, opportunity: i64) -> i64 {
    tx.flush().await.unwrap();
    tx.query(
        "SELECT COUNT(*)::int8 FROM mail_activity a
              JOIN ir_model_data d ON d.res_id=a.activity_type_id AND d.model='mail.activity.type'
              WHERE d.module='sale_crm' AND d.name='activity_review_cancelled_booking'
                AND a.res_model='crm.lead' AND a.res_id=$1 AND a.active",
        &[&opportunity],
    )
    .await
    .unwrap()[0]
        .get(0)
}

#[test]
fn confirming_and_cancelling_multiple_linked_orders_updates_the_deal_once() {
    run(async {
        let (db, system, _) = cloned_sale_system().await;
        // Installing CRM after Sales must activate the bridge on an existing database.
        let install = Box::pin(system.button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("crm").unwrap()],
        ))
        .await
        .unwrap();
        assert!(install.post_commit.is_clean());
        let orm = install.orm;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let forecast = xmlid(&mut tx, "crm", "forecast_report").await;
        let booking_line = xmlid(&mut tx, "sale_crm", "forecast_bookings").await;
        let booking_expr = xmlid(&mut tx, "sale_crm", "forecast_booking_amount").await;
        let line = tx
            .env()
            .records("reporting.report.line")
            .unwrap()
            .browse([booking_line])
            .unwrap();
        assert_eq!(
            line.cell(&mut tx, "report_id").await.unwrap(),
            Cell::Id(forecast)
        );
        assert_eq!(
            line.cell(&mut tx, "groupby").await.unwrap(),
            Cell::Str("team_id,user_id".into())
        );
        let expr = tx
            .env()
            .records("reporting.report.expression")
            .unwrap()
            .browse([booking_expr])
            .unwrap();
        assert_eq!(
            expr.cell(&mut tx, "field").await.unwrap(),
            Cell::Str("crm_booking_amount".into())
        );
        assert_eq!(
            expr.cell(&mut tx, "date_field").await.unwrap(),
            Cell::Str("date_order".into())
        );
        let world = world(&mut tx).await;
        admin(&mut tx).await;
        let seller = user(&mut tx, "crm-integration-sales", "group_sale_salesman").await;
        let outsider = user(&mut tx, "crm-outsider", "group_sale_salesman").await;
        let team = team(&mut tx, "CRM direct", seller).await;
        let env = tx.env().with_company(world.company).unwrap();
        let lead = env
            .records("crm.lead")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Expansion".into()))
                        .set_cell("kind", Cell::Str("opportunity".into()))
                        .set_cell("user_id", Cell::Id(seller))
                        .set_cell("team_id", Cell::Id(team))
                        .set_cell(
                            "expected_revenue",
                            Cell::Decimal(erp_fields::Decimal::from(1800)),
                        )
                        .set_cell("partner_id", Cell::Id(world.customer)),
                ],
            )
            .await
            .unwrap();
        let deal = lead.ids()[0];
        let mut docs = Vec::new();
        for _ in 0..2 {
            let action: Value = lead
                .call(&mut tx, "action_create_quotation", ())
                .await
                .unwrap();
            let id = returned_form(&action, "sale.order");
            let order = env.records("sale.order").unwrap().browse([id]).unwrap();
            for (field, expected) in [
                ("opportunity_id", deal),
                ("partner_id", world.customer),
                ("company_id", world.company),
                ("user_id", seller),
                ("team_id", team),
            ] {
                assert_eq!(
                    order.cell(&mut tx, field).await.unwrap(),
                    Cell::Id(expected)
                );
            }
            assert_eq!(text(&mut tx, &order, "state").await, "draft");
            let reverse: Value = order
                .call(&mut tx, "action_view_opportunity", ())
                .await
                .unwrap();
            assert_eq!(returned_form(&reverse, "crm.lead"), deal);
            docs.push(order);
        }
        assert_ne!(docs[0].ids(), docs[1].ids());
        let other = partner(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("Other buyer".into())),
        )
        .await;
        write(
            &mut tx,
            &docs[0],
            RawVals::new().set_cell("partner_id", Cell::Id(other)),
        )
        .await
        .unwrap_err();
        assert_eq!(
            docs[0].cell(&mut tx, "partner_id").await.unwrap(),
            Cell::Id(world.customer)
        );

        let tax = prices::tax(&mut tx, world.company, "CRM sales tax", "10").await;
        let product = prices::product(&mut tx, "CRM service", "250", &[tax]).await;
        prices::add_line(
            &mut tx,
            &docs[0],
            RawVals::new().set_cell("product_id", Cell::Id(product)),
        )
        .await;
        assert_eq!(
            docs[0].cell(&mut tx, "crm_booking_amount").await.unwrap(),
            Cell::Decimal(0.into())
        );
        act(&mut tx, &docs[0], "action_confirm").await.unwrap();
        assert_eq!(text(&mut tx, &lead, "state").await, "won");
        assert_eq!(
            docs[0].cell(&mut tx, "crm_booking_amount").await.unwrap(),
            Cell::Decimal(250.into())
        );
        assert_eq!(
            docs[0].cell(&mut tx, "amount_total").await.unwrap(),
            Cell::Decimal(275.into())
        );
        let evidence = text(&mut tx, &lead, "close_reason").await;
        assert!(evidence.contains(&text(&mut tx, &docs[0], "name").await));
        assert_eq!(
            lead.cell(&mut tx, "won_amount").await.unwrap(),
            Cell::Decimal(1800.into())
        );
        act(&mut tx, &docs[1], "action_confirm").await.unwrap();
        assert_eq!(text(&mut tx, &lead, "close_reason").await, evidence);
        write(
            &mut tx,
            &docs[0],
            RawVals::new().set_cell("opportunity_id", Cell::Null),
        )
        .await
        .unwrap_err();
        let first_order = docs[0].ids()[0];
        let final_order = docs[1].ids()[0];
        tx.commit().await.unwrap();
        drop(tx);

        // This transaction sees both bookings before the other session cancels the first.
        let mut stale = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(review_count(&mut stale, deal).await, 0);
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let first = tx
            .env()
            .with_company(world.company)
            .unwrap()
            .records("sale.order")
            .unwrap()
            .browse([first_order])
            .unwrap();
        act(&mut tx, &first, "action_cancel").await.unwrap();
        assert_eq!(review_count(&mut tx, deal).await, 0);
        assert_eq!(
            first.cell(&mut tx, "crm_booking_amount").await.unwrap(),
            Cell::Decimal(0.into())
        );
        tx.commit().await.unwrap();
        drop(tx);
        let last = stale
            .env()
            .with_company(world.company)
            .unwrap()
            .records("sale.order")
            .unwrap()
            .browse([final_order])
            .unwrap();
        let retry = act(&mut stale, &last, "action_cancel").await.unwrap_err();
        assert_eq!(retry.sqlstate(), Some("40001"));
        stale.rollback().await.unwrap();
        drop(stale);

        let policy = policy_orm(&db, &orm).await;
        let mut tx = policy
            .begin(erp_orm::UserId::new(outsider).unwrap())
            .await
            .unwrap();
        let env = tx.env().with_company(world.company).unwrap();
        let hidden = env.records("crm.lead").unwrap().browse([deal]).unwrap();
        let error = hidden
            .call::<_, Value>(&mut tx, "action_create_quotation", ())
            .await
            .unwrap_err();
        assert_eq!(error.kind().http_status(), 403);
        let hidden_order = env
            .records("sale.order")
            .unwrap()
            .browse([final_order])
            .unwrap();
        assert_eq!(
            act(&mut tx, &hidden_order, "action_cancel")
                .await
                .unwrap_err()
                .kind()
                .http_status(),
            403
        );
        assert_eq!(review_count(&mut tx, deal).await, 0);
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = policy
            .begin(erp_orm::UserId::new(seller).unwrap())
            .await
            .unwrap();
        let own = tx
            .env()
            .with_company(world.company)
            .unwrap()
            .records("crm.lead")
            .unwrap()
            .browse([deal])
            .unwrap();
        assert_eq!(
            own.cell(&mut tx, "quotation_count").await.unwrap(),
            Cell::Int(2)
        );
        let order = own
            .env()
            .records("sale.order")
            .unwrap()
            .browse([final_order])
            .unwrap();
        act(&mut tx, &order, "action_cancel").await.unwrap();
        act(&mut tx, &order, "action_cancel").await.unwrap();
        assert_eq!(review_count(&mut tx, deal).await, 1);
        let review = own
            .env()
            .records("mail.activity")
            .unwrap()
            .search(
                &mut tx,
                &(erp_sdk::Domain::field("res_model").unwrap().eq("crm.lead")
                    & erp_sdk::Domain::field("res_id").unwrap().eq(deal)),
            )
            .await
            .unwrap();
        assert_eq!(review.len(), 1);
        assert_eq!(
            review.cell(&mut tx, "user_id").await.unwrap(),
            Cell::Id(seller)
        );
        assert_eq!(text(&mut tx, &own, "state").await, "won");
        tx.rollback().await.unwrap();
        drop(tx);
        drop(policy);
        drop(orm);
        drop(system);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn a_sales_manager_uses_the_team_directory_for_quotas_under_private_account_rules() {
    run(async {
        let (db, system, _) = cloned_sale_system().await;
        let install = Box::pin(system.button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("crm").unwrap()],
        ))
        .await
        .unwrap();
        let orm = install.orm;
        let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut setup).await;
        admin(&mut setup).await;
        let manager = user(&mut setup, "crm-quota-manager", "group_sale_manager").await;
        let seller = user(&mut setup, "crm-quota-rep", "group_sale_salesman").await;
        let sale_team = team(&mut setup, "Quota team", seller).await;
        let other_team = team(&mut setup, "Other quota team", manager).await;
        let member = setup
            .env()
            .records("crm.team.member")
            .unwrap()
            .search(
                &mut setup,
                &(erp_sdk::Domain::field("crm_team_id").unwrap().eq(sale_team)
                    & erp_sdk::Domain::field("user_id").unwrap().eq(seller)),
            )
            .await
            .unwrap()
            .int()
            .unwrap();
        setup.commit().await.unwrap();
        let policy = policy_orm(&db, &orm).await;
        let mut tx = policy
            .begin_with(
                erp_orm::UserId::new(manager).unwrap(),
                Context::new().with_company_id(world.company),
            )
            .await
            .unwrap();
        let env = tx.env();
        assert_eq!(
            env.records("res.users")
                .unwrap()
                .browse([seller])
                .unwrap()
                .check_access(&mut tx, erp_sdk::Operation::Read)
                .await
                .unwrap_err()
                .kind()
                .http_status(),
            403
        );
        let matches = env
            .records("crm.team.member")
            .unwrap()
            .name_search_with(
                &mut tx,
                "crm-quota-rep",
                &erp_orm::NameSearch::new()
                    .domain(erp_sdk::Domain::field("crm_team_id").unwrap().eq(sale_team)),
            )
            .await
            .unwrap();
        assert_eq!(matches, vec![(member, Some("crm-quota-rep".into()))]);
        let model = env.records("crm.quota").unwrap();
        let fields = RawVals::new()
            .set_cell("company_id", Cell::Id(world.company))
            .set_cell(
                "month",
                Cell::Date(erp_fields::Date::from_ymd(2026, 1, 1).unwrap()),
            )
            .set_cell("amount", Cell::Decimal(100.into()))
            .set_cell("salesperson_member_id", Cell::Id(member));
        let invalid = fields.clone().set_cell("team_id", Cell::Id(other_team));
        let rejected = tx
            .savepoint(async |tx| model.create(tx, vec![invalid]).await)
            .await;
        assert!(
            rejected
                .unwrap_err()
                .to_string()
                .contains("membership must match")
        );
        let quota = model.create(&mut tx, vec![fields]).await.unwrap();
        assert_eq!(
            quota.cell(&mut tx, "team_id").await.unwrap(),
            Cell::Id(sale_team)
        );
        assert_eq!(
            quota.cell(&mut tx, "user_id").await.unwrap(),
            Cell::Id(seller)
        );
        write(
            &mut tx,
            &quota,
            RawVals::new()
                .set_cell("team_id", Cell::Id(other_team))
                .set_cell("user_id", Cell::Id(manager)),
        )
        .await
        .unwrap();
        assert_ne!(
            quota.cell(&mut tx, "salesperson_member_id").await.unwrap(),
            Cell::Id(member)
        );
        assert!(matches!(
            quota.cell(&mut tx, "salesperson_member_id").await.unwrap(),
            Cell::Id(_)
        ));
        tx.rollback().await.unwrap();
    });
}
