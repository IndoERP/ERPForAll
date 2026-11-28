// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installation, workflow rollback, repeated qualification and two-session sales ownership.
use super::*;

async fn install_crm() -> (TestDb, Orm) {
    let (db, system, _) = cloned_sale_system().await;
    let installed = Box::pin(system.button_immediate_install(
        &system.system_env(Context::new()),
        &[ModuleName::new("crm").unwrap()],
    ))
    .await
    .unwrap();
    assert!(installed.post_commit.is_clean());
    (db, installed.orm)
}

fn action_record(action: &Value) -> i64 {
    match action {
        Value::Json(j) => j["res_id"].as_i64().expect("form record"),
        Value::Map(m) => match m.get("res_id") {
            Some(Value::Int(id)) => *id,
            _ => panic!("missing form record"),
        },
        _ => panic!("expected a window action"),
    }
}

async fn qualify_business_twice(tx: &mut Transaction, env: &erp_sdk::Env) {
    let leads = env.records("crm.lead").unwrap();
    let wizards = env.records("crm.qualify.wizard").unwrap();
    let initial = RawVals::new()
        .set_cell("name", Cell::Str("Office equipment".into()))
        .set_cell("company_name", Cell::Str("Example Business".into()))
        .set_cell(
            "contact_name",
            Cell::Str("Purchasing representative".into()),
        )
        .set_cell("email", Cell::Str("Purchasing@Example.test".into()));
    let first = leads.create(tx, vec![initial]).await.unwrap();
    let action: Value = first.call(tx, "action_qualify", ()).await.unwrap();
    let wizard = wizards.browse([action_record(&action)]).unwrap();
    assert_eq!(
        wizard.cell(tx, "choice").await.unwrap(),
        Cell::Str("business".into())
    );
    let _: Value = wizard.call(tx, "action_convert", ()).await.unwrap();
    let account = first.related(tx, "partner_id").await.unwrap();
    assert_eq!(
        account.cell(tx, "crm_is_business").await.unwrap(),
        Cell::Bool(true)
    );
    let role = first.related(tx, "contact_role_ids").await.unwrap();
    let contact = role
        .ensure_one()
        .unwrap()
        .related(tx, "partner_id")
        .await
        .unwrap();
    assert_ne!(contact.int().unwrap(), account.int().unwrap());
    assert_eq!(
        contact.cell(tx, "parent_id").await.unwrap(),
        Cell::Id(account.int().unwrap())
    );

    let repeat = leads
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Additional office equipment".into()))
                    .set_cell("email", Cell::Str("purchasing@example.test".into())),
            ],
        )
        .await
        .unwrap();
    let action: Value = repeat.call(tx, "action_qualify", ()).await.unwrap();
    let wizard = wizards.browse([action_record(&action)]).unwrap();
    assert_eq!(
        wizard.cell(tx, "choice").await.unwrap(),
        Cell::Str("existing".into())
    );
    assert_eq!(
        wizard.cell(tx, "partner_id").await.unwrap(),
        Cell::Id(contact.int().unwrap())
    );
    let _: Value = wizard.call(tx, "action_convert", ()).await.unwrap();
    let _: Value = wizard.call(tx, "action_convert", ()).await.unwrap();
    assert_eq!(
        repeat.cell(tx, "partner_id").await.unwrap(),
        Cell::Id(account.int().unwrap())
    );
    let repeated_role = repeat.related(tx, "contact_role_ids").await.unwrap();
    assert_eq!(
        repeated_role
            .ensure_one()
            .unwrap()
            .cell(tx, "partner_id")
            .await
            .unwrap(),
        Cell::Id(contact.int().unwrap())
    );
}

#[test]
fn qualification_preserves_the_deal_and_enforces_stage_requirements_on_write() {
    run(async {
        let (_db, orm) = install_crm().await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let world = world(&mut tx).await;
        let env = tx.env().with_company(world.company).unwrap();
        let leads = env.records("crm.lead").unwrap();
        qualify_business_twice(&mut tx, &env).await;
        let expected_stage = xmlid(&mut tx, "crm", "stage_prospect").await;
        let expected_pipeline = xmlid(&mut tx, "crm", "pipeline_direct").await;
        let Cell::Id(currency) = env
            .records("res.company")
            .unwrap()
            .browse([world.company])
            .unwrap()
            .cell(&mut tx, "currency_id")
            .await
            .unwrap()
        else {
            panic!("company currency");
        };
        let want = [
            "stage_id",
            "pipeline_id",
            "probability",
            "forecast_category",
            "deal_currency_id",
        ]
        .map(str::to_owned)
        .to_vec();
        let defaults: erp_sdk::ValueMap = leads
            .call(&mut tx, "default_get", (want.clone(),))
            .await
            .unwrap();
        assert_eq!(defaults["stage_id"], Value::Int(expected_stage));
        assert_eq!(defaults["pipeline_id"], Value::Int(expected_pipeline));
        assert_eq!(defaults["deal_currency_id"], Value::Int(currency));
        assert_eq!(defaults["probability"], Value::Int(10));
        let channel = env
            .records("crm.pipeline")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Channel".into()))],
            )
            .await
            .unwrap();
        let channel_stage = env
            .records("crm.stage")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Prospecting".into()))
                        .set_cell("pipeline_id", Cell::Id(channel.int().unwrap()))
                        .set_cell("probability", Cell::Int(42)),
                ],
            )
            .await
            .unwrap();
        let map = |entries: Vec<(String, Value)>| {
            Value::Map(std::sync::Arc::new(
                entries.into_iter().collect::<erp_sdk::ValueMap>(),
            ))
        };
        let spec = map(want
            .iter()
            .map(|field| (field.clone(), map(Vec::new())))
            .collect());
        let values = map(vec![
            ("stage_id".to_owned(), Value::Int(expected_stage)),
            ("pipeline_id".to_owned(), Value::Int(channel.int().unwrap())),
        ]);
        let changed = leads
            .web_call(
                &mut tx,
                "onchange",
                &map(vec![
                    ("values".to_owned(), values),
                    (
                        "field_names".to_owned(),
                        Value::list([Value::str("pipeline_id")]),
                    ),
                    ("fields_spec".to_owned(), spec),
                ]),
            )
            .await
            .unwrap();
        let result = changed.dict().unwrap().get("value").unwrap();
        assert_eq!(
            result.dict().unwrap().get("stage_id").unwrap(),
            Value::Int(channel_stage.int().unwrap())
        );
        let team = env
            .records("crm.team")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Channel sales".into()))
                        .set_cell("crm_pipeline_id", Cell::Id(channel.int().unwrap()))
                        .set_commands("crm_pipeline_ids", [Command::set([id(expected_pipeline)])]),
                ],
            )
            .await
            .unwrap();
        let intake = leads.with_context([("default_team_id", Value::Int(team.int().unwrap()))]);
        let legacy = intake
            .create(
                &mut tx,
                vec![
                    RawVals::new().set_cell("name", Cell::Str("Historical channel enquiry".into())),
                ],
            )
            .await
            .unwrap();
        channel
            .write(
                &mut tx,
                RawVals::new().set_cell("active", Cell::Bool(false)),
            )
            .await
            .unwrap();
        assert_eq!(
            legacy.cell(&mut tx, "pipeline_id").await.unwrap(),
            Cell::Id(channel.int().unwrap())
        );
        let current = intake
            .default_get(&mut tx, &["pipeline_id", "stage_id"])
            .await
            .unwrap();
        assert_eq!(current["pipeline_id"], Value::Int(expected_pipeline));
        assert_eq!(current["stage_id"], Value::Int(expected_stage));
        assert!(
            intake
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("Archived channel enquiry".into()))
                            .set_cell("pipeline_id", Cell::Id(channel.int().unwrap()))
                    ]
                )
                .await
                .is_err()
        );
        team.write(
            &mut tx,
            RawVals::new().set_commands("crm_pipeline_ids", [Command::set(Vec::<RecId>::new())]),
        )
        .await
        .unwrap();
        assert!(intake.default_get(&mut tx, &["pipeline_id"]).await.is_err());
        team.write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
        let lead = leads
            .with_env(env.with_context([("default_probability", Value::Int(26))]))
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Independent buyer".into()))
                        .set_cell("contact_name", Cell::Str("Buyer".into()))
                        .set_cell("phone", Cell::Str("+62 123 456789".into())),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            lead.cell(&mut tx, "probability").await.unwrap(),
            Cell::Int(26)
        );
        let amount = erp_fields::Decimal::from(160);
        let stage = xmlid(&mut tx, "crm", "stage_negotiate").await;
        let wizard_action: Value = lead.call(&mut tx, "action_qualify", ()).await.unwrap();
        let wizard = env
            .records("crm.qualify.wizard")
            .unwrap()
            .browse([action_record(&wizard_action)])
            .unwrap();
        let action: Value = wizard.call(&mut tx, "action_convert", ()).await.unwrap();
        assert_eq!(action_record(&action), lead.int().unwrap());
        let customer = lead.cell(&mut tx, "partner_id").await.unwrap();
        assert!(matches!(customer, Cell::Id(_)));
        let again: Value = wizard.call(&mut tx, "action_convert", ()).await.unwrap();
        assert_eq!(action_record(&again), lead.int().unwrap());
        assert_eq!(lead.cell(&mut tx, "partner_id").await.unwrap(), customer);
        assert_eq!(
            lead.cell(&mut tx, "originated_as_lead").await.unwrap(),
            Cell::Bool(true)
        );
        let initial_stage = lead.cell(&mut tx, "stage_id").await.unwrap();
        assert!(
            lead.write(
                &mut tx,
                RawVals::new().set_cell("stage_id", Cell::Id(stage))
            )
            .await
            .is_err()
        );
        assert_eq!(lead.cell(&mut tx, "stage_id").await.unwrap(), initial_stage);
        lead.write(
            &mut tx,
            RawVals::new()
                .set_cell("stage_id", Cell::Id(stage))
                .set_cell("probability", Cell::Int(68))
                .set_cell("date_deadline", Cell::Date(erp_fields::Date::today()))
                .set_cell("expected_revenue", Cell::Decimal(amount)),
        )
        .await
        .unwrap();
        assert_eq!(
            lead.cell(&mut tx, "probability").await.unwrap(),
            Cell::Int(68)
        );
        assert_eq!(
            lead.cell(&mut tx, "forecast_category").await.unwrap(),
            Cell::Str("commit".into())
        );
        // A manual win needs an accountable salesperson; automatic confirmation may supply one.
        assert!(
            lead.call::<_, ()>(&mut tx, "action_win", ("Signed contract".to_owned(),))
                .await
                .is_err()
        );
        admin(&mut tx).await;
        let seller = user(&mut tx, "crm.manual.winner", "group_sale_salesman").await;
        lead.write(
            &mut tx,
            RawVals::new().set_cell("user_id", Cell::Id(seller)),
        )
        .await
        .unwrap();
        let _: () = lead
            .call(
                &mut tx,
                "action_win",
                ("Signed order outside ERP".to_owned(),),
            )
            .await
            .unwrap();
        assert_eq!(
            lead.cell(&mut tx, "won_amount").await.unwrap(),
            Cell::Decimal(amount)
        );
        let _: () = lead
            .call(&mut tx, "action_win", ("Retry".to_owned(),))
            .await
            .unwrap();
        let _: () = lead.call(&mut tx, "action_reopen", ()).await.unwrap();
        assert_eq!(
            lead.cell(&mut tx, "state").await.unwrap(),
            Cell::Str("open".into())
        );
        let history = env
            .records("crm.stage.history")
            .unwrap()
            .search(
                &mut tx,
                &erp_sdk::Domain::field("lead_id")
                    .unwrap()
                    .eq(lead.int().unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(history.len(), 3);
        let retained = env
            .records("crm.stage.history")
            .unwrap()
            .search(
                &mut tx,
                &(erp_sdk::Domain::field("lead_id")
                    .unwrap()
                    .eq(lead.int().unwrap())
                    & erp_sdk::Domain::field("outcome").unwrap().eq("won")),
            )
            .await
            .unwrap();
        assert_eq!(
            retained
                .ensure_one()
                .unwrap()
                .cell(&mut tx, "closing_amount")
                .await
                .unwrap(),
            Cell::Decimal(amount)
        );
        tx.rollback().await.unwrap();
    });
}

#[test]
fn a_second_claim_cannot_steal_a_locked_or_already_owned_lead() {
    run(async {
        let (db, installed) = install_crm().await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let w = world(&mut setup).await;
        admin(&mut setup).await;
        let first = user(&mut setup, "crm.claim.one", "group_sale_salesman").await;
        let second = user(&mut setup, "crm.claim.two", "group_sale_salesman").await;
        let tid = team(&mut setup, "Shared inbox", first).await;
        setup
            .env()
            .records("crm.team.member")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("crm_team_id", Cell::Id(tid))
                        .set_cell("user_id", Cell::Id(second)),
                ],
            )
            .await
            .unwrap();
        let queue = setup
            .env()
            .with_company(w.company)
            .unwrap()
            .records("crm.lead")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Shared queue".into()))
                        .set_cell("team_id", Cell::Id(tid)),
                ],
            )
            .await
            .unwrap()
            .int()
            .unwrap();
        setup.commit().await.unwrap();
        let orm = policy_orm(&db, &installed).await;
        let ctx = Context::new().with_company_id(w.company);
        let mut left = orm
            .begin_with(erp_orm::UserId::new(first).unwrap(), ctx.clone())
            .await
            .unwrap();
        let mut right = orm
            .begin_with(erp_orm::UserId::new(second).unwrap(), ctx.clone())
            .await
            .unwrap();
        let a = left
            .env()
            .records("crm.lead")
            .unwrap()
            .browse([queue])
            .unwrap();
        let b = right
            .env()
            .records("crm.lead")
            .unwrap()
            .browse([queue])
            .unwrap();
        let _: () = a.call(&mut left, "action_claim", ()).await.unwrap();
        let err = b
            .call::<_, ()>(&mut right, "action_claim", ())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Lock);
        right.rollback().await.unwrap();
        left.commit().await.unwrap();
        let mut retry = orm
            .begin_with(erp_orm::UserId::new(second).unwrap(), ctx)
            .await
            .unwrap();
        let b = retry
            .env()
            .records("crm.lead")
            .unwrap()
            .browse([queue])
            .unwrap();
        assert!(
            b.call::<_, ()>(&mut retry, "action_claim", ())
                .await
                .is_err()
        );
        retry.rollback().await.unwrap();
        let mut audit = installed.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            audit
                .env()
                .records("crm.lead")
                .unwrap()
                .browse([queue])
                .unwrap()
                .cell(&mut audit, "user_id")
                .await
                .unwrap(),
            Cell::Id(first)
        );
        audit.rollback().await.unwrap();
    });
}

#[test]
fn routing_uses_the_matching_team_and_never_exposes_another_companys_pipeline() {
    run(async {
        let (db, installed) = install_crm().await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let w = world(&mut setup).await;
        admin(&mut setup).await;
        let first = user(&mut setup, "route.first", "group_sale_salesman_all_leads").await;
        let second = user(&mut setup, "route.second", "group_sale_salesman").await;
        let team_id = team(&mut setup, "Distribution", first).await;
        let env = setup.env().with_company(w.company).unwrap();
        let second_member = env
            .records("crm.team.member")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("crm_team_id", Cell::Id(team_id))
                        .set_cell("user_id", Cell::Id(second)),
                ],
            )
            .await
            .unwrap()
            .int()
            .unwrap();
        let rule = env
            .records("crm.rule")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Web enquiries".into()))
                        .set_cell("source", Cell::Str("Web".into()))
                        .set_cell("team_id", Cell::Id(team_id))
                        .set_cell("company_id", Cell::Id(w.company))
                        .set_cell("points", Cell::Int(35)),
                ],
            )
            .await
            .unwrap();
        let leads = env.records("crm.lead").unwrap();
        let routed = leads
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Web prospect 1".into()))
                        .set_cell("source", Cell::Str("web".into())),
                    RawVals::new()
                        .set_cell("name", Cell::Str("Web prospect 2".into()))
                        .set_cell("source", Cell::Str(" Web ".into())),
                ],
            )
            .await
            .unwrap();
        let mut assignees = routed.cells(&mut setup, "user_id").await.unwrap();
        assert_eq!(assignees.remove(0), Cell::Id(first));
        assert_eq!(assignees, vec![Cell::Id(second)]);
        assert_eq!(
            routed.cells(&mut setup, "score").await.unwrap(),
            vec![Cell::Int(35); 2]
        );
        // Action defaults participate in routing exactly as values entered on the form.
        let defaulted = leads.with_context([("default_source", Value::str("web"))]);
        let r = defaulted
            .create(
                &mut setup,
                vec![RawVals::new().set_cell("name", Cell::Str("Inbound from campaign".into()))],
            )
            .await
            .unwrap();
        assert_eq!(
            r.cell(&mut setup, "user_id").await.unwrap(),
            Cell::Id(first)
        );
        assert_eq!(
            rule.cell(&mut setup, "cursor_position").await.unwrap(),
            Cell::Int(1)
        );
        rule.write(&mut setup, RawVals::new().set_cell("points", Cell::Int(12)))
            .await
            .unwrap();
        assert_eq!(
            routed.cells(&mut setup, "score").await.unwrap(),
            vec![Cell::Int(12); 2]
        );
        env.records("crm.team")
            .unwrap()
            .browse([team_id])
            .unwrap()
            .write(
                &mut setup,
                RawVals::new().set_cell("active", Cell::Bool(false)),
            )
            .await
            .unwrap();
        let queued = defaulted
            .create(
                &mut setup,
                vec![
                    RawVals::new().set_cell("name", Cell::Str("Inbound after team archive".into())),
                ],
            )
            .await
            .unwrap();
        assert_ne!(
            queued.cell(&mut setup, "team_id").await.unwrap(),
            Cell::Id(team_id)
        );
        assert_eq!(
            queued.cell(&mut setup, "user_id").await.unwrap(),
            Cell::Null
        );
        assert_eq!(
            queued.cell(&mut setup, "score").await.unwrap(),
            Cell::Int(12)
        );
        env.records("crm.team")
            .unwrap()
            .browse([team_id])
            .unwrap()
            .write(
                &mut setup,
                RawVals::new().set_cell("active", Cell::Bool(true)),
            )
            .await
            .unwrap();
        let companies = setup.env().records("res.company").unwrap();
        let currency = companies
            .browse([w.company])
            .unwrap()
            .cell(&mut setup, "currency_id")
            .await
            .unwrap();
        let other = companies
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Other CRM tenant".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .int()
            .unwrap();
        let other_env = env.with_company(other).unwrap();
        let foreign = other_env
            .records("crm.lead")
            .unwrap()
            .create(
                &mut setup,
                vec![RawVals::new().set_cell("name", Cell::Str("Private opportunity".into()))],
            )
            .await
            .unwrap()
            .int()
            .unwrap();
        setup.commit().await.unwrap();
        let orm = policy_orm(&db, &installed).await;
        let mut personal = orm
            .begin_with(
                erp_orm::UserId::new(second).unwrap(),
                Context::new().with_company_id(w.company),
            )
            .await
            .unwrap();
        let intake = personal.env().records("crm.lead").unwrap();
        let interactive = intake.with_context([("crm_personal_form", Value::Bool(true))]);
        let mut own = interactive
            .default_get(&mut personal, &["user_id"])
            .await
            .unwrap();
        assert_eq!(own.get("user_id"), Some(&Value::Int(second)));
        own.insert("name".to_owned(), Value::str("Personal lead"));
        own.insert("source".to_owned(), Value::str("Web"));
        let own = interactive
            .web_save(
                &mut personal,
                erp_orm::WebSave {
                    vals: own,
                    specification: erp_orm::Spec::from_value(&Value::Json(std::sync::Arc::new(
                        serde_json::json!({"user_id":{}, "name":{}}),
                    )))
                    .unwrap(),
                    next_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            own[0].dict().unwrap().get("user_id").unwrap(),
            Value::Int(second)
        );
        assert_eq!(
            own[0].dict().unwrap().get("name").unwrap(),
            Value::str("Personal lead")
        );
        let from_rep = intake
            .with_context([
                ("default_source", Value::str("web")),
                ("crm_personal_form", Value::Bool(true)),
                ("import_file", Value::Bool(true)),
            ])
            .create(
                &mut personal,
                vec![
                    RawVals::new().set_cell("name", Cell::Str("Submitted by sales".into())),
                    RawVals::new().set_cell("name", Cell::Str("Submitted for a colleague".into())),
                ],
            )
            .await
            .unwrap();
        let mine = intake
            .search(
                &mut personal,
                &erp_sdk::Domain::condition("id", erp_sdk::Operator::In, from_rep.ids()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            mine.ensure_one()
                .unwrap()
                .cell(&mut personal, "user_id")
                .await
                .unwrap(),
            Cell::Id(second)
        );
        let from_rep = from_rep.ids();
        let submitted_name = "Routed wizard submission for colleague";
        let submitted = erp_sdk::Domain::field("name").unwrap().eq(submitted_name);
        for name in ["Routed wizard submission for self", submitted_name] {
            let wizard = personal
                .env()
                .records("crm.submit.wizard")
                .unwrap()
                .create(
                    &mut personal,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("source", Cell::Str("Web".into())),
                    ],
                )
                .await
                .unwrap();
            let receipt: Value = wizard
                .call(&mut personal, "action_submit", ())
                .await
                .unwrap();
            let Value::Json(body) = &receipt else {
                panic!("submission receipt")
            };
            assert_eq!(body["tag"], "display_notification");
            assert!(body.get("res_id").is_none());
            let again: Value = wizard
                .call(&mut personal, "action_submit", ())
                .await
                .unwrap();
            assert_eq!(receipt, again);
            assert!(
                wizard
                    .write(
                        &mut personal,
                        RawVals::new()
                            .set_cell("name", Cell::Str("Changed after submission".into()))
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            intake
                .search_count(&mut personal, &submitted, None)
                .await
                .unwrap(),
            0
        );
        personal.commit().await.unwrap();
        let mut active = orm
            .begin_with(
                erp_orm::UserId::new(first).unwrap(),
                Context::new().with_company_id(w.company),
            )
            .await
            .unwrap();
        let visible = active
            .env()
            .records("crm.lead")
            .unwrap()
            .search(
                &mut active,
                &erp_sdk::Domain::condition(
                    "id",
                    erp_sdk::Operator::In,
                    [routed.ids(), vec![foreign]].concat(),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .ids();
        assert_eq!(visible.len(), 2);
        assert!(!visible.contains(&foreign));
        let received = active
            .env()
            .records("crm.lead")
            .unwrap()
            .search(&mut active, &submitted)
            .await
            .unwrap();
        assert_eq!(
            received
                .ensure_one()
                .unwrap()
                .cell(&mut active, "user_id")
                .await
                .unwrap(),
            Cell::Id(first)
        );
        let owners = active
            .env()
            .records("crm.lead")
            .unwrap()
            .browse(from_rep)
            .unwrap()
            .cells(&mut active, "user_id")
            .await
            .unwrap();
        assert!(owners.contains(&Cell::Id(first)));
        assert!(owners.contains(&Cell::Id(second)));
        // A sales manager can assign from the visible team directory without reading private accounts.
        let assigned = active
            .env()
            .records("crm.lead")
            .unwrap()
            .browse([routed.ids()[0]])
            .unwrap();
        assigned
            .write(
                &mut active,
                RawVals::new().set_cell("salesperson_member_id", Cell::Id(second_member)),
            )
            .await
            .unwrap();
        assert_eq!(
            assigned.cell(&mut active, "user_id").await.unwrap(),
            Cell::Id(second)
        );
        assert_eq!(
            assigned.cell(&mut active, "team_id").await.unwrap(),
            Cell::Id(team_id)
        );
        active.rollback().await.unwrap();
    });
}
