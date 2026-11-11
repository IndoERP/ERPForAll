//! Native seed-protection extension contract. Installed seed/cascade acceptance follows.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use erp_testkit::Harness;
#[path = "../../../addons/uom/tests/support/guard_fixture.rs"]
mod guard_fixture;

mod tighter_units {
    use erp_addon_uom::Uom;
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "uom_guard_test", depends = ["uom"]);

    #[extend(Uom, addon = "uom_guard_test")]
    struct TighterUnits {}

    #[methods(Uom, addon = "uom_guard_test")]
    impl TighterUnits {
        #[method(override)]
        async fn _unprotected_uom_xml_ids(
            this: &RecordSet<Uom>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<Vec<String>> {
            let mut defaults: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
            defaults.retain(|name| name != "product_uom_hour");
            Ok(defaults)
        }
    }
}

#[tokio::test]
async fn base_exemptions_are_the_specified_three_xml_id_names() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let result: Vec<String> = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .call(&mut tx, "_unprotected_uom_xml_ids", ())
        .await
        .expect("native seed-protection extension service must exist");
    assert_eq!(
        result,
        [
            "product_uom_hour",
            "product_uom_dozen",
            "product_uom_pack_6"
        ]
        .map(str::to_owned)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn extension_preserves_super_defaults_and_can_remove_an_exemption() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &tighter_units::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let result: Vec<String> = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .call(&mut tx, "_unprotected_uom_xml_ids", ())
        .await
        .expect("override must compose with the native UoM base implementation");
    assert_eq!(
        result,
        ["product_uom_dozen", "product_uom_pack_6"].map(str::to_owned)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn seed_fixture() -> Harness {
    let visible = erp_sdk::Domain::condition("active", erp_sdk::Operator::Eq, true).unwrap();
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule("uom.uom", erp_sdk::Operation::Read, visible.clone())
        .rule("uom.uom", erp_sdk::Operation::Unlink, visible)
        .build()
        .unwrap();
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .policy(policy)
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .await
    .unwrap();
    guard_fixture::install(&h).await;
    h
}

#[tokio::test]
async fn protected_archived_descendant_refuses_entire_cascading_delete() {
    use erp_sdk::prelude::*;
    let h = seed_fixture().await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users(id,name,login,active) VALUES(42,'Guard caller','guard-caller',true)",
    )
    .await
    .unwrap();
    let units = tx.env().records("uom.uom").unwrap();
    let root = units
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Custom tree".into()))],
        )
        .await
        .unwrap();
    let child = units
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Hidden historical child".into()))
                    .set_cell("relative_uom_id", Cell::Id(root.ids()[0]))
                    .set_cell("active", Cell::Bool(false)),
            ],
        )
        .await
        .unwrap();
    tx.execute("INSERT INTO ir_model_data(module,name,model,res_id) VALUES ('uom','protected_child','uom.uom',$1::bigint)", &[&child.ids()[0]]).await.unwrap();
    let error = root.with_user(42).unlink(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "System units of measure cannot be deleted. Archive them instead."
    );
    let remaining: i64 = tx.query("SELECT count(*) FROM uom_uom", &[]).await.unwrap()[0].get(0);
    assert_eq!(remaining, 2);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_exempt_alias_never_overrides_a_protected_alias() {
    use erp_sdk::prelude::*;
    let h = seed_fixture().await;
    let mut tx = h.begin().await.unwrap();
    let units = tx.env().records("uom.uom").unwrap();
    let protected = units
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Alias unit".into()))],
        )
        .await
        .unwrap();
    tx.execute("INSERT INTO ir_model_data(module,name,model,res_id) VALUES ('uom','product_uom_hour','uom.uom',$1::bigint),('uom','protected_alias','uom.uom',$1::bigint)", &[&protected.ids()[0]]).await.unwrap();
    let error = tx
        .savepoint(async |tx| protected.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "System units of measure cannot be deleted. Archive them instead."
    );
    let custom = units
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Other module unit".into()))],
        )
        .await
        .unwrap();
    tx.execute("INSERT INTO ir_model_data(module,name,model,res_id) VALUES ('other_module','product_uom_unit','uom.uom',$1::bigint)", &[&custom.ids()[0]]).await.unwrap();
    custom.unlink(&mut tx).await.unwrap();
    let remaining: i64 = tx.query("SELECT count(*) FROM uom_uom", &[]).await.unwrap()[0].get(0);
    assert_eq!(remaining, 1);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn removing_the_hour_exemption_changes_actual_deletion() {
    use erp_sdk::prelude::*;
    for tighter in [false, true] {
        let mut addons = vec![
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_data::model_data::ADDON,
        ];
        if tighter {
            addons.push(&tighter_units::ADDON);
        }
        let h = Harness::builder(addons)
            .with_decls([erp_data::ir_model_data_decl()])
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .build()
            .await
            .unwrap();
        guard_fixture::install(&h).await;
        let mut tx = h.begin().await.unwrap();
        let record = tx
            .env()
            .records("uom.uom")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Hour".into()))],
            )
            .await
            .unwrap();
        tx.execute("INSERT INTO ir_model_data(module,name,model,res_id) VALUES('uom','product_uom_hour','uom.uom',$1::bigint)",&[&record.ids()[0]]).await.unwrap();
        if tighter {
            let error = record.unlink(&mut tx).await.unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
            assert_eq!(
                error.message(),
                "System units of measure cannot be deleted. Archive them instead."
            );
        } else {
            record.unlink(&mut tx).await.unwrap();
            assert_eq!(
                tx.query("SELECT count(*) FROM uom_uom", &[]).await.unwrap()[0].get::<_, i64>(0),
                0
            );
        }
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}
