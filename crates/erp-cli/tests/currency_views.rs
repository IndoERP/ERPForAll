//! Currency screens of the installed base addon: Settings > Currencies and the views a currency or a
//! currency rate opens with from a related field.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use erp_fields::Value;
use erp_modules::{
    AddonIndex,
    system::{InitOptions, ModuleSystem},
};
use erp_orm::rpc::{Exposure, RpcArgs};
use erp_orm::{Orm, SUPERUSER_ID};
use erp_testkit::TestDb;

fn repo_addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

async fn installed(db: &TestDb) -> Orm {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(repo_addons());
    // Explicit provenance for this binary's private native fixture bytes.
    let native_artifact_identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(native_artifact_identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = system.initialize(InitOptions::new()).await.unwrap();
    assert!(init.data_notices.is_empty(), "{:?}", init.data_notices);
    init.orm
}

/// `get_views` without view ids, the call a related field makes when it opens a record.
async fn default_views(orm: &Orm, model: &str, types: &[&str]) -> serde_json::Value {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let records = tx.env().records(model).unwrap();
    let Exposure::Exposed(method) = tx
        .exposure()
        .get(records.model_id(), "get_views")
        .unwrap()
        .clone()
    else {
        panic!("get_views is exposed on {model}")
    };
    let views = types
        .iter()
        .map(|t| Value::List(vec![Value::False, Value::str(*t)].into()))
        .collect::<Vec<_>>();
    let args = erp_fields::ValueMap::from_iter([
        ("views".into(), Value::List(views.into())),
        ("wire_major".into(), Value::Int(1)),
        (
            "options".into(),
            Value::Map(Arc::new(erp_fields::ValueMap::from_iter([(
                "mobile".into(),
                Value::Bool(false),
            )]))),
        ),
    ]);
    let value = method
        .call(&mut tx, records, RpcArgs::new(args))
        .await
        .unwrap_or_else(|e| panic!("{model} get_views: {e:?}"));
    tx.rollback().await.unwrap();
    erp_fields::convert::value_to_json(&value).unwrap()
}

/// Field names of one served view, in arch order.
fn field_names(view: &serde_json::Value) -> Vec<String> {
    view["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["field"]["name"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn currency_and_rate_open_with_their_default_views() {
    let db = TestDb::create().await.unwrap();
    let orm = installed(&db).await;

    let currency = default_views(&orm, "res.currency", &["form", "list"]).await;
    let views = currency["views"].as_array().unwrap();
    assert_eq!(views.len(), 2, "{currency}");
    let form = field_names(&views[0]);
    for field in [
        "name",
        "full_name",
        "symbol",
        "position",
        "rounding",
        "decimal_places",
        "active",
        "rate_ids",
    ] {
        assert!(
            form.contains(&field.to_owned()),
            "currency form lacks {field}: {form:?}"
        );
    }
    let list = field_names(&views[1]);
    assert_eq!(
        list,
        ["name", "symbol", "full_name", "active"],
        "{currency}"
    );

    let rate = default_views(&orm, "res.currency.rate", &["list", "form"]).await;
    let views = rate["views"].as_array().unwrap();
    assert_eq!(views.len(), 2, "{rate}");
    assert_eq!(field_names(&views[0]), ["name", "rate", "company_id"]);
    assert_eq!(
        field_names(&views[1]),
        ["name", "rate", "currency_id", "company_id"]
    );
}

#[tokio::test]
async fn currencies_menu_sits_under_settings_for_administrators() {
    let db = TestDb::create().await.unwrap();
    let orm = installed(&db).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT m.id \
               FROM ir_ui_menu m \
               JOIN ir_model_data dm ON dm.model = 'ir.ui.menu' AND dm.res_id = m.id \
               JOIN ir_ui_menu parent ON parent.id = m.parent_id \
               JOIN ir_model_data dp ON dp.model = 'ir.ui.menu' AND dp.res_id = parent.id \
               JOIN ir_ui_menu_group_rel r ON r.menu_id = m.id \
               JOIN ir_model_data dg ON dg.model = 'res.groups' AND dg.res_id = r.gid \
              WHERE dm.module = 'base' AND dm.name = 'settings_currencies_menu' \
                AND dp.module = 'base' AND dp.name = 'settings_menu' \
                AND dg.module = 'base' AND dg.name = 'group_system'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "menu, parent and group link");
    tx.rollback().await.unwrap();
}
