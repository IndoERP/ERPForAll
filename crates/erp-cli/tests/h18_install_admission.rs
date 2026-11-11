//! H18 addon dependency checks run on actual module data writes, including inline action views.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_sdk::Context;
use erp_testkit::TestDb;

// Test-only registered nonbuiltin: no renderer behavior is exercised by lifecycle admission.
fn kanban_type() -> erp_sdk::views::ViewResult<erp_sdk::views::ViewTypeDecl> {
    use erp_views::*;
    static ROOT: SchemaNode = SchemaNode::Object(&[]);
    fn validate(_: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
        budget.check()
    }
    fn build(
        _: &CheckedViewInput<'_>,
        root: DescriptionValueBuilder<'_>,
    ) -> ViewResult<DescriptionValue> {
        root.object(|_| Ok(()))
    }
    ViewTypeDecl::builder("web", "kanban")
        .wire_major(1)
        .renderer("kanban")
        .schema(&ViewSchema::new(1, &ROOT)?)
        .validator(validate)
        .builder(build)
        .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
        .methods(&[])
        .finish()
}
static EXTRA_TYPES: [erp_sdk::views::ViewTypeDeclFn; 1] = [kanban_type];

fn orm_error<'a>(error: &'a (dyn std::error::Error + 'static)) -> &'a erp_sdk::Error {
    if let Some(error) = error.downcast_ref::<erp_sdk::Error>() {
        return error;
    }
    match error.source() {
        Some(source) => orm_error(source),
        None => panic!("expected an ORM error in {error:?}"),
    }
}

/// installs refuse missing owner dependency in window modes, inline action views and view records)
#[test]
fn module_data_requires_owner_dependencies_beyond_the_three_framework_builtins() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(install_admission());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn install_admission() {
    let db = TestDb::create().await.unwrap();
    let fixtures = tempfile::tempdir().unwrap();
    let fixture_root = fixtures.path().canonicalize().unwrap();
    let cases = [
        (
            "h18_bad_action",
            "<record id='bad' model='ir.actions.act_window'><field name='name'>Bad H18 action</field><field name='res_model'>res.partner</field><field name='view_mode'>kanban</field></record>",
        ),
        (
            "h18_bad_write",
            "<record id='bad' model='ir.actions.act_window'><field name='name'>Bad H18 write</field><field name='res_model'>res.partner</field><field name='view_mode'>form</field></record><record id='bad' model='ir.actions.act_window'><field name='view_mode'>kanban</field></record>",
        ),
        (
            "h18_bad_nameless",
            r#"<record model='ir.actions.act_window' context="{'__erp_data_module':'web'}"><field name='name'>Bad H18 nameless</field><field name='res_model'>res.partner</field><field name='view_mode'>kanban</field></record>"#,
        ),
        (
            "h18_bad_fn_create",
            r#"<function model="ir.actions.act_window" name="create"><value eval="{'name':'Forged H18 function create','res_model':'res.partner','view_mode':'kanban'}"/><value name="context" eval="{'__erp_data_module':'web','ordinary_key':'retained'}"/></function>"#,
        ),
        (
            "h18_bad_fn_write",
            r#"<record id="target" model="ir.actions.act_window"><field name="name">Forged H18 function write</field><field name="res_model">res.partner</field><field name="view_mode">form</field></record><function model="ir.actions.act_window" name="write"><value eval="[ref('target')]"/><value eval="{'view_mode':'kanban'}"/><value name="context" eval="{'__erp_data_module':'web'}"/></function>"#,
        ),
        (
            "h18_bad_fn_nested",
            r#"<function model="ir.actions.act_window" name="write"><function model="ir.actions.act_window" name="create"><value eval="{'name':'Forged H18 nested function','res_model':'res.partner','view_mode':'kanban'}"/><value name="context" eval="{'__erp_data_module':'web'}"/></function><value eval="{'name':'Forged H18 outer function'}"/></function>"#,
        ),
        (
            "h18_bad_inline",
            "<record id='bad' model='ir.actions.act_window'><field name='name'>Bad H18 inline</field><field name='res_model'>res.partner</field><field name='view_mode'>form</field><field name='view_ids' eval=\"[(0,0,{'view_mode':'kanban'})]\"/></record>",
        ),
        (
            "h18_bad_view",
            "<record id='bad' model='ir.ui.view'><field name='name'>Bad H18 view</field><field name='model'>res.partner</field><field name='arch' type='xml'><kanban><field name='name'/></kanban></field></record>",
        ),
    ];
    for (module, xml) in cases {
        let dir = fixture_root.join(module);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.toml"),
            "name='H18 dependency fixture'\ndepends=['base']\ndata=['data.xml']\n",
        )
        .unwrap();
        std::fs::write(dir.join("data.xml"), format!("<odoo>{xml}</odoo>")).unwrap();
    }
    let good = fixture_root.join("h18_good_owner");
    std::fs::create_dir(&good).unwrap();
    std::fs::write(
        good.join("manifest.toml"),
        "name='H18 valid owner fixture'\ndepends=['web']\ndata=['data.xml']\n",
    )
    .unwrap();
    std::fs::write(good.join("data.xml"), "<odoo><record id='good' model='ir.actions.act_window'><field name='name'>Valid H18 owner</field><field name='res_model'>res.partner</field><field name='view_mode'>kanban</field></record></odoo>").unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    index.add_dev_path(fixture_root);
    let identities = index
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
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .view_types(erp_addon_web::view_type_contributions().into_iter().chain([
            erp_sdk::views::ViewTypeContribution::new(&erp_addon_web::ADDON, &EXTRA_TYPES),
        ]))
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    // Base's own form/list records and web search records are framework builtins, so this is also
    // the positive actual-install proof for the lead-approved builtin exemption.
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    assert!(initialized.post_commit.is_clean());
    let good_caller = system.system_env(Context::new());
    let good = system
        .button_immediate_install(&good_caller, &[ModuleName::new("h18_good_owner").unwrap()])
        .await
        .unwrap();
    assert!(good.post_commit.is_clean());
    let client = db.client().await.unwrap();
    let mode: String = client.query_one("SELECT view_mode FROM ir_act_window WHERE id=(SELECT res_id FROM ir_model_data WHERE module='h18_good_owner' AND name='good')", &[]).await.unwrap().get(0);
    assert_eq!(mode, "kanban");
    const RECORD_COUNTS: &str = "SELECT (SELECT count(*) FROM ir_act_window), (SELECT count(*) FROM ir_act_window_view), (SELECT count(*) FROM ir_ui_view)";
    let baseline = client.query_one(RECORD_COUNTS, &[]).await.unwrap();
    let expected_counts: [i64; 3] = [baseline.get(0), baseline.get(1), baseline.get(2)];
    drop(client);
    for (module, _) in cases {
        let caller = system.system_env(Context::new());
        let result = system
            .button_immediate_install(&caller, &[ModuleName::new(module).unwrap()])
            .await;
        let error = match result {
            Ok(_) => panic!("module {module} installed without its view-type owner's dependency"),
            Err(error) => error,
        };
        let error = orm_error(&error);
        eprintln!(
            "h18_install_refusal module={module} kind={:?} message={}",
            error.kind(),
            error.message()
        );
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            format!(
                "View-type key \"kanban\" belongs to addon \"web\", which is not in the installed dependency closure of addon \"{module}\""
            )
        );
        let client = db.client().await.unwrap();
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM ir_model_data WHERE module=$1",
                &[&module],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 0, "refused module data must roll back together");
        let forged: i64 = client
            .query_one(
                "SELECT count(*) FROM ir_act_window WHERE name LIKE 'Forged H18%'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            forged, 0,
            "function-created or updated actions must roll back with refused installation"
        );
        let actual = client.query_one(RECORD_COUNTS, &[]).await.unwrap();
        let actual_counts: [i64; 3] = [actual.get(0), actual.get(1), actual.get(2)];
        assert_eq!(
            actual_counts, expected_counts,
            "failed {module} must leave no physical window, inline action-view or view row, including idless records"
        );
        let state: String = client
            .query_one(
                "SELECT state FROM ir_module_module WHERE name=$1",
                &[&module],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            state, "uninstalled",
            "the failed install cannot publish its module state"
        );
        eprintln!(
            "h18_install_rollback module={module} physical={actual_counts:?} expected={expected_counts:?} external_ids={count} state={state}"
        );
    }
}
