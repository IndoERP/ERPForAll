//! Every view the real `base` and `web` addons install is served by `get_view` (
//! ): the capture of the guards is stricter than the
//! install-time validation (it resolves the fields of sub-views against their comodels), so a
//! shipped arch it refuses would only show at serve time.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_addon_base::ir_ui_view::get_view;
use erp_addon_base::ir_ui_view::select::{ViewId, ViewRequest, ViewType};
use erp_modules::AddonIndex;
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_orm::{Orm, UserId};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::TestDb;

/// superuser; per-user filtering is `spec_data_060_groups_in_arch_filter_elements_per_user`)
#[test]
fn every_installed_primary_view_is_served() {
    // the module install needs more than the default test-thread stack (as `human_base_bootstrap`)
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(corpus());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn corpus() {
    let db = TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    let native_artifact_identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                std::path::Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(native_artifact_identities)
        // Discovery checks hooks for every manifest, including uninstalled addons.
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

    // the compiled policy decides the guards (the module system's own ORM has no group-aware policy)
    let source = PolicySource::build(Arc::clone(init.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let resolver = Resolver::for_registry(init.orm.registry()).unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(init.orm.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(resolver)
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    // an administrator (the Technical screens are for `base.group_system`): the guards are
    // decided on this user's groups, so a restricted node or view is served or refused for real
    let client = db.client().await.unwrap();
    let admin: i64 = client
        .query_one(
            "INSERT INTO res_users (name, login, active, share) \
             VALUES ('corpus-admin', 'corpus-admin', true, false) RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let inserted = client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) \
             SELECT $1, res_id FROM ir_model_data \
              WHERE module = 'base' AND name = 'group_system' AND model = 'res.groups'",
            &[&admin],
        )
        .await
        .unwrap();
    assert_eq!(inserted, 1, "base.group_system exists once");
    let mut tx = orm.begin(UserId::new(admin).unwrap()).await.unwrap();
    let rows = tx
        .query(
            "SELECT id, model, type FROM ir_ui_view \
              WHERE mode = 'primary' AND active AND model IS NOT NULL AND type IS NOT NULL \
              ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    let views: Vec<(i64, String, String)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    // the technical screens alone are 18 models with a list, a form and a search view each
    assert!(
        views.len() >= 54,
        "only {} primary views installed",
        views.len()
    );

    // superuser mode: some technical models have no read grant at all (`ir.config_parameter`), and
    // the model check is not what is under test; the guards still follow the administrator's groups
    let env = tx.env().sudo(true);
    let mut types = std::collections::BTreeSet::new();
    for (id, model, view_type) in &views {
        let ty = ViewType::new(&tx, view_type).unwrap();
        let req = ViewRequest::new(model, &ty).view_id(ViewId::new(*id).unwrap());
        let served = get_view(&mut tx, &env, &req)
            .await
            .unwrap_or_else(|e| panic!("view {id} ({model}, {view_type}): {e:?}"));
        assert_eq!(served.view_id().get(), *id);
        assert!(!served.arch().is_empty(), "view {id} is served empty");
        types.insert(view_type.clone());
    }
    assert!(
        ["list", "form", "search"]
            .iter()
            .all(|t| types.contains(*t)),
        "{types:?}"
    );
    tx.rollback().await.unwrap();
    drop(orm);
    drop(init.orm);
    drop(system);
    db.cleanup().await.unwrap();
}
