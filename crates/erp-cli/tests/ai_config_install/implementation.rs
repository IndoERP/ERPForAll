// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;

#[test]
fn implementation_views_render_with_the_native_framework() -> Result<(), Box<dyn std::error::Error>>
{
    use erp_data::{DataOperation, FieldValue, parse_xml_str};
    use erp_views::{OwnerBoundViewTypes, ViewCatalogue, ViewDescriptor};
    let registry = Arc::new(
        erp_registry::RegistryBuilder::new([
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_ai_base::ADDON,
        ])
        .build()?,
    );
    let renderers = OwnerBoundViewTypes::bind(
        Arc::clone(&registry),
        &erp_addon_web::view_type_contributions(),
    )?;
    let xml = include_str!("../../../../addons/ai_base/views/implementation.xml");
    let mut checked = 0;
    for operation in parse_xml_str(xml, "implementation.xml", "ai_base", false)? {
        let DataOperation::Record(record) = operation else {
            continue;
        };
        if record.model() != "ir.ui.view" {
            continue;
        }
        let name = record.id().ok_or("Missing view id")?.name();
        let field = |key| record.field(key).map(|f| f.value());
        let (Some(FieldValue::Xml(arch)), Some(FieldValue::Text(model))) =
            (field("arch"), field("model"))
        else {
            return Err(format!("Missing view definition on {name}").into());
        };
        let catalogue = ViewCatalogue::new(vec![ViewDescriptor::new(1, name, model, arch)])?;
        let combined = catalogue.combine(1)?;
        let kind = combined.arch().root().tag();
        let decl = renderers
            .declarations()
            .find(|(_, decl)| decl.key() == kind)
            .ok_or("Missing native renderer")?
            .1;
        let spec = registry
            .model(model)
            .ok_or("Missing implementation model")?;
        erp_addon_web::metadata_test_support::test_native_layout(combined.arch(), spec, decl)
            .map_err(|error| format!("{name}: {error}"))?;
        checked += 1;
    }
    assert_eq!(checked, 6);
    Ok(())
}

#[test]
fn scoped_project_resumes_and_import_receipt_is_atomic() {
    use erp_addon_ai_base::implementation::StepImport;
    use erp_sdk::Value;
    big(async {
        let db = TestDb::create().await.unwrap();
        let retail::Retail {
            installed,
            orm,
            company,
            users,
        } = retail::install(&db).await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let admin = UserId::new(
            setup
                .query("SELECT id FROM res_users WHERE login='catalog-base'", &[])
                .await
                .unwrap()[0]
                .get(0),
        )
        .unwrap();
        let other_company: i64 = setup.query("INSERT INTO res_company (name,currency_id,tz) SELECT 'Second implementation',currency_id,'UTC' FROM res_company WHERE id=$1 RETURNING id::int8",&[&company]).await.unwrap()[0].get(0);
        setup.commit().await.unwrap();
        let context =
            Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(company)]))]);
        let mut tx = orm.begin_with(admin, context.clone()).await.unwrap();
        let projects = tx.env().records("ai.implementation.project").unwrap();
        let start = || {
            (
                "Implementation".to_owned(),
                "Use purchasing and inventory".to_owned(),
            )
        };
        let first: Value = projects
            .call(&mut tx, "start_implementation", start())
            .await
            .unwrap();
        let again: Value = projects
            .call(&mut tx, "start_implementation", start())
            .await
            .unwrap();
        assert_eq!(first, again);
        let project = projects
            .search(
                &mut tx,
                &Domain::field("name").unwrap().eq("Implementation"),
            )
            .await
            .unwrap();
        let steps = tx.env().records("ai.implementation.step").unwrap();
        let purchase = steps
            .search(
                &mut tx,
                &(Domain::field("project_id")
                    .unwrap()
                    .eq(project.int().unwrap())
                    & Domain::field("addon").unwrap().eq("purchase")),
            )
            .await
            .unwrap();
        let id = purchase.int().unwrap();
        let company_step = steps
            .search(
                &mut tx,
                &(Domain::field("project_id")
                    .unwrap()
                    .eq(project.int().unwrap())
                    & Domain::field("addon").unwrap().eq("base")),
            )
            .await
            .unwrap()
            .int()
            .unwrap();
        let opened: Value = purchase
            .call(&mut tx, "action_open_data", ())
            .await
            .unwrap();
        let Value::Json(opened) = opened else {
            unreachable!("expected native action")
        };
        assert_eq!(opened["context"]["ai_implementation_step_id"], id);
        let model = opened["res_model"].as_str().unwrap().to_owned();
        let done: bool = purchase
            .call(
                &mut tx,
                "record_progress",
                (
                    "done".to_owned(),
                    "Signed purchase acceptance #12".to_owned(),
                ),
            )
            .await
            .unwrap();
        assert!(done);
        tx.commit().await.unwrap();

        // The same principal's setup actions must return ordinary records and compare the
        // settings revision before applying a write. No HTTP provider credentials are used.
        // Establish an earlier snapshot in another connection before the winning writer commits.
        let mut racing = orm.begin_with(admin, context.clone()).await.unwrap();
        racing
            .query("SELECT count(*) FROM ai_implementation_contact_key", &[])
            .await
            .unwrap();
        let mut master = orm.begin_with(admin, context.clone()).await.unwrap();
        let contacts = master.env().records("res.partner").unwrap();
        let contact: Value = contacts
            .call(
                &mut master,
                "create_setup_contact",
                (
                    "Implementation sample".to_owned(),
                    "implementation-sample-1".to_owned(),
                    "before@example.test".to_owned(),
                    false,
                ),
            )
            .await
            .unwrap();
        let Value::Json(contact) = contact else {
            unreachable!("expected a contact action")
        };
        let contact_id = contact["res_id"].as_i64().unwrap();
        let partner = contacts.browse([contact_id]).unwrap();
        let changed: bool = partner
            .call(
                &mut master,
                "update_setup_contact_email",
                (
                    "before@example.test".to_owned(),
                    "after@example.test".to_owned(),
                ),
            )
            .await
            .unwrap();
        assert!(changed);
        assert_eq!(
            partner.cell(&mut master, "email").await.unwrap(),
            Cell::Str("after@example.test".into())
        );
        let settings = master.env().records("ir.config.setting").unwrap();
        let snapshot: Value = settings
            .call(&mut master, "list_company_setup", ())
            .await
            .unwrap();
        let Value::Json(snapshot) = snapshot else {
            unreachable!("expected company Settings")
        };
        let revision = snapshot["settings"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let saved: Value = settings
            .call(
                &mut master,
                "set_company_setup_value",
                (
                    "purchase.days_to_purchase".to_owned(),
                    revision.clone(),
                    "3".to_owned(),
                ),
            )
            .await
            .unwrap();
        let Value::Json(saved) = saved else {
            unreachable!("expected applied Settings")
        };
        assert_eq!(saved["values"][0]["value"], 3);
        master.commit().await.unwrap();

        let contacts = racing.env().records("res.partner").unwrap();
        assert!(
            contacts
                .call::<_, Value>(
                    &mut racing,
                    "create_setup_contact",
                    (
                        "Racing".to_owned(),
                        "implementation-sample-1".to_owned(),
                        String::new(),
                        false
                    )
                )
                .await
                .is_err()
        );
        racing.rollback().await.unwrap();
        let mut edit = orm.begin_with(admin, context.clone()).await.unwrap();
        let contacts = edit.env().records("res.partner").unwrap();
        contacts
            .browse([contact_id])
            .unwrap()
            .write(
                &mut edit,
                RawVals::new().set_cell("ref", Cell::Str("renamed-reference".into())),
            )
            .await
            .unwrap();
        assert_eq!(
            contacts
                .search_count(
                    &mut edit,
                    &Domain::field("ref").unwrap().eq("implementation-sample-1"),
                    None
                )
                .await
                .unwrap(),
            0
        );
        edit.commit().await.unwrap();
        let mut stale = orm.begin_with(admin, context.clone()).await.unwrap();
        let settings = stale.env().records("ir.config.setting").unwrap();
        assert!(
            settings
                .call::<_, Value>(
                    &mut stale,
                    "set_company_setup_value",
                    (
                        "purchase.days_to_purchase".to_owned(),
                        revision,
                        "4".to_owned()
                    )
                )
                .await
                .is_err()
        );
        stale.rollback().await.unwrap();

        let mut foreign = orm.begin_with(admin, context.clone()).await.unwrap();
        let contacts = foreign.env().records("res.partner").unwrap();
        let duplicate = contacts
            .call::<_, Value>(
                &mut foreign,
                "create_setup_contact",
                (
                    "Another".to_owned(),
                    "implementation-sample-1".to_owned(),
                    String::new(),
                    false,
                ),
            )
            .await;
        assert!(matches!(duplicate,Err(error) if error.message().contains("already exists")));
        foreign.rollback().await.unwrap();

        let mut wrong = orm.begin_with(admin, context.clone()).await.unwrap();
        let project_model = wrong.env().records("ai.implementation.project").unwrap();
        assert!(
            project_model
                .create(
                    &mut wrong,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("Wrong company".into()))
                            .set_cell("company_id", Cell::Id(other_company))
                    ]
                )
                .await
                .is_err()
        );
        wrong.rollback().await.unwrap();

        let mut buyer = orm.begin_with(users[0], context.clone()).await.unwrap();
        let blind = buyer
            .env()
            .records("ai.implementation.step")
            .unwrap()
            .browse([id])
            .unwrap();
        assert!(
            blind
                .call::<(), Value>(&mut buyer, "action_open_data", ())
                .await
                .is_err()
        );
        buyer.rollback().await.unwrap();

        // Governed browser imports resume at the native load RPC after admission.
        // A successful call must write the same receipt seen by a subsequent file upload.
        let scoped = context
            .clone()
            .with("ai_implementation_step_id", Value::Int(company_step));
        let cols = ["id", "name"];
        let raw = vec![vec![
            "base.main_company".to_owned(),
            "Implemented Company".to_owned(),
        ]];
        let mut rpc = orm.begin_with(admin, scoped.clone()).await.unwrap();
        let companies = rpc.env().records("res.company").unwrap();
        let erp_orm::rpc::Exposure::Exposed(load) = rpc
            .exposure()
            .get(companies.model_id(), "load")
            .unwrap()
            .clone()
        else {
            unreachable!("expected native load")
        };
        let args = erp_fields::ValueMap::from_iter([
            ("fields".into(), Value::list(cols.iter().map(Value::str))),
            (
                "data".into(),
                Value::list(
                    raw.iter()
                        .map(|row| Value::list(row.iter().map(Value::str))),
                ),
            ),
        ]);
        let result = load
            .call(&mut rpc, companies, erp_orm::rpc::RpcArgs::new(args))
            .await
            .unwrap();
        assert_eq!(
            erp_fields::convert::value_to_json(&result).unwrap()["ids"],
            serde_json::json!([company])
        );
        rpc.commit().await.unwrap();
        let mut retry = orm.begin_with(admin, scoped).await.unwrap();
        let duplicate = StepImport::prepare(&mut retry, "res.company", &cols, &raw).await;
        assert!(
            matches!(duplicate, Err(error) if error.message().contains("already been imported"))
        );
        retry.rollback().await.unwrap();

        let context = context.with("ai_implementation_step_id", Value::Int(id));
        let fields = ["name"];
        let rows = vec![vec!["Example".to_owned()]];
        let mut failed_import = orm.begin_with(admin, context.clone()).await.unwrap();
        let receipt = StepImport::prepare(&mut failed_import, &model, &fields, &rows)
            .await
            .unwrap()
            .unwrap();
        receipt.complete(&mut failed_import, 1).await.unwrap();
        failed_import.rollback().await.unwrap();
        let mut imported = orm.begin_with(admin, context.clone()).await.unwrap();
        StepImport::prepare(&mut imported, &model, &fields, &rows)
            .await
            .unwrap()
            .unwrap()
            .complete(&mut imported, 1)
            .await
            .unwrap();
        imported.commit().await.unwrap();
        let mut retry = orm.begin_with(admin, context).await.unwrap();
        let duplicate = StepImport::prepare(&mut retry, &model, &fields, &rows).await;
        assert!(
            matches!(duplicate,Err(error) if error.message().contains("already been imported"))
        );
        retry.rollback().await.unwrap();
    });
}

#[test]
fn implementation_tools_have_precise_opt_in_rpc_schemas() {
    let registry = Arc::new(
        erp_registry::RegistryBuilder::new([&erp_addon_base::ADDON, &erp_addon_ai_base::ADDON])
            .build()
            .unwrap(),
    );
    let pool = erp_orm::pool::create_pool(erp_testkit::tokio_postgres::Config::new(), 1).unwrap();
    let orm = Orm::builder(registry, pool).build().unwrap();
    let offered = |model: &str, method: &str| {
        orm.exposure()
            .assistant_offered(orm.registry().model_id(model).unwrap(), method)
            .is_ok()
    };
    for (model, method) in [
        ("ai.implementation.project", "start_implementation"),
        ("ai.implementation.project", "implementation_steps"),
        ("ai.implementation.project", "action_refresh_steps"),
        ("ai.implementation.project", "action_open_settings"),
        ("ai.implementation.step", "action_open_data"),
        ("ai.implementation.step", "record_fit_gap"),
        ("ai.implementation.step", "record_progress"),
        ("ir.config.setting", "list_company_setup"),
        ("ir.config.setting", "set_company_setup_value"),
        ("res.partner", "create_setup_contact"),
        ("res.partner", "update_setup_contact_email"),
    ] {
        assert!(offered(model, method), "missing {model}.{method}");
    }
    for method in ["erp_write_values", "erp_toggle_group"] {
        assert!(!offered("ir.config.setting", method));
    }
    assert!(!offered("res.partner", "write"));
}
