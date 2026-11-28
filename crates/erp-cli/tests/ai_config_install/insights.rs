// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;
use erp_sdk::{Operation, Value};
use serde_json::json;

#[test]
fn saved_definitions_follow_group_membership_and_source_rights_on_every_open() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let retail::Retail {
            installed,
            orm,
            company,
            users,
        } = retail::install(&db).await;
        let mut setup = installed.begin(SUPERUSER_ID).await.unwrap();
        let group: i64 = setup.query("SELECT res_id FROM ir_model_data WHERE module='purchase' AND name='group_purchase_user'", &[]).await.unwrap()[0].get(0);
        let admin = UserId::new(
            setup
                .query("SELECT id FROM res_users WHERE login='catalog-base'", &[])
                .await
                .unwrap()[0]
                .get(0),
        )
        .unwrap();
        let sales_group: i64 = setup.query("SELECT res_id FROM ir_model_data WHERE module='sales_team' AND name='group_sale_salesman'", &[]).await.unwrap()[0].get(0);
        setup
            .env()
            .records("res.users")
            .unwrap()
            .browse([admin.get()])
            .unwrap()
            .write(
                &mut setup,
                RawVals::new().set_commands(
                    "group_ids",
                    [Command::link(RecId::from_real(sales_group).unwrap())],
                ),
            )
            .await
            .unwrap();
        setup.commit().await.unwrap();
        let def = json!({"panels":[{"title":"Sales","model":"sale.order","groupby":["state"],"aggregates":["id:count"],"domain":[],"limit":8,"order":""}]}).to_string();
        let mut owner = orm.begin(admin).await.unwrap();
        let model = owner.env().records("ai.insight").unwrap();
        let Value::Json(action) = erp_orm::call_method::<(String, String), Value>(
            &mut owner,
            &model,
            "save_analysis",
            ("Sales dashboard".into(), def.clone()),
        )
        .await
        .unwrap() else {
            panic!("expected a native form action for the saved insight");
        };
        let (destination, id) = erp_addon_ai_base::operation::destination(&action).unwrap();
        assert_eq!(destination, "ai.insight");
        assert!(id > 0);
        assert_eq!(action["views"], json!([[false, "form"]]));
        let saved = model.browse([id]).unwrap();
        let Value::Json(live) =
            erp_orm::call_method::<(), Value>(&mut owner, &saved, "erp_analysis_queries", ())
                .await
                .unwrap()
        else {
            panic!("expected a JSON report definition");
        };
        assert_eq!(
            live["panels"][0]["args"]["context"]["allowed_company_ids"],
            json!([company])
        );
        assert_eq!(live["panels"][0]["method"], "web_read_group");
        owner.commit().await.unwrap();
        let mut reader = orm.begin(users[0]).await.unwrap();
        let private = reader
            .env()
            .records("ai.insight")
            .unwrap()
            .browse([id])
            .unwrap();
        assert!(
            !private
                .has_access(&mut reader, Operation::Read)
                .await
                .unwrap()
        );
        reader.rollback().await.unwrap();

        let mut owner = orm.begin(admin).await.unwrap();
        let shared = owner
            .env()
            .records("ai.insight")
            .unwrap()
            .browse([id])
            .unwrap();
        shared
            .write(
                &mut owner,
                RawVals::new().set_cell("shared_group_id", Cell::Id(group)),
            )
            .await
            .unwrap();
        owner.commit().await.unwrap();
        let mut reader = orm.begin(users[0]).await.unwrap();
        let shared = reader
            .env()
            .records("ai.insight")
            .unwrap()
            .browse([id])
            .unwrap();
        assert!(
            shared
                .has_access(&mut reader, Operation::Read)
                .await
                .unwrap()
        );
        assert!(
            !shared
                .has_access(&mut reader, Operation::Write)
                .await
                .unwrap()
        );
        // The buyer can read this definition but the sales source is checked using the buyer's current rights.
        let source_error =
            erp_orm::call_method::<(), Value>(&mut reader, &shared, "erp_analysis_queries", ())
                .await
                .unwrap_err();
        assert_eq!(source_error.kind(), ErrorKind::Access);
        reader.rollback().await.unwrap();

        let mut sales = orm.begin(users[1]).await.unwrap();
        let foreign = sales
            .env()
            .records("ai.insight")
            .unwrap()
            .browse([id])
            .unwrap();
        assert!(
            !foreign
                .has_access(&mut sales, Operation::Read)
                .await
                .unwrap()
        );
        let own = sales.env().records("ai.insight").unwrap();
        assert!(
            own.create(
                &mut sales,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Forbidden publication".into()))
                        .set_cell("definition", Cell::Str(def.into()))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("shared_group_id", Cell::Id(group))
                ]
            )
            .await
            .is_err()
        );
        sales.rollback().await.unwrap();
        let mut owner = orm.begin(admin).await.unwrap();
        let shared = owner
            .env()
            .records("ai.insight")
            .unwrap()
            .browse([id])
            .unwrap();
        shared
            .write(
                &mut owner,
                RawVals::new().set_cell("shared_group_id", Cell::Null),
            )
            .await
            .unwrap();
        owner.commit().await.unwrap();
        let mut reader = orm.begin(users[0]).await.unwrap();
        let private = reader
            .env()
            .records("ai.insight")
            .unwrap()
            .browse([id])
            .unwrap();
        assert!(
            !private
                .has_access(&mut reader, Operation::Read)
                .await
                .unwrap()
        );
        reader.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}
