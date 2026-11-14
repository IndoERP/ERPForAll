//! Real native-base bootstrap foundations; HTTP/password activation is tested by the server.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

#[path = "support/base_catalog.rs"]
mod base_catalog;
#[path = "support/original_currency_ids.rs"]
mod original_currency_ids;

use erp_modules::AddonIndex;
use erp_modules::manifest::ModuleName;
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_sdk::{Context, RegistryBuilder};
use erp_testkit::TestDb;

#[test]
fn partner_identity_is_in_the_native_base_registry() {
    let registry = RegistryBuilder::new([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    let partner = registry
        .model("res.partner")
        .expect("native base partner model");
    assert!(partner.field("name").is_some());
    assert!(partner.field("email").is_some());
    assert!(partner.field("active").is_some());
    assert!(partner.field("company_id").is_some());
    assert!(partner.field("country_id").is_some());
    assert!(partner.field("state_id").is_some());
    let user = registry.model("res.users").unwrap();
    let link = user
        .field("partner_id")
        .expect("nullable legacy user partner link");
    assert!(!link.required);
    assert_eq!(link.ty.comodel(), Some("res.partner"));
}

#[test]
fn real_base_manifest_initializes_groups_company_and_explicit_acl_only() {
    // The unoptimized base initialization chain needs more than the 2 MiB test thread (same reason
    // as `real_base_manifest_initializes_technical_menu_and_views`; the framework fix is
    // ).
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(groups_company_and_explicit_acl_only());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn groups_company_and_explicit_acl_only() {
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
    // Explicit provenance for this binary's private native fixture bytes.
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
        .code([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert!(init.data_notices.is_empty(), "{:?}", init.data_notices);
    assert!(init.post_commit.failed.is_empty());
    let client = db.client().await.unwrap();
    let group_names = [
        "group_user",
        "group_user_regular",
        "group_portal",
        "group_public",
        "group_erp_manager",
        "default_user_group",
        "group_system",
    ];
    let before = client.query(
        "SELECT name, res_id FROM ir_model_data WHERE module = 'base' AND name = ANY($1) ORDER BY name",
        &[&group_names.as_slice()]).await.unwrap();
    assert_eq!(before.len(), group_names.len());
    let identities: Vec<(String, i64)> =
        before.iter().map(|row| (row.get(0), row.get(1))).collect();
    let company = client
        .query_one(
            "SELECT c.id, c.name, r.name FROM res_company c JOIN ir_model_data d ON d.res_id=c.id \
         AND d.model='res.company' JOIN res_currency r ON r.id=c.currency_id \
         WHERE d.module='base' AND d.name='main_company'",
            &[],
        )
        .await
        .unwrap();
    let company_id: i64 = company.get(0);
    assert_eq!(company.get::<_, String>(2), "USD");
    let rows = client.query(
        "SELECT m.model, a.operation, g.name FROM ir_access a JOIN ir_model m ON m.id=a.model_id \
         JOIN ir_model_data g ON g.model='res.groups' AND g.res_id=a.group_id AND g.module='base' \
         ORDER BY m.model, a.operation, g.name", &[]).await.unwrap();
    assert!(!rows.is_empty(), "base must ship real ACL data");
    // The typed Settings facade admits update RPC entry; each key/scope still authorizes itself.
    let settings_operations: Vec<String> = rows
        .iter()
        .filter(|row| {
            row.get::<_, String>(0) == "ir.config.setting"
                && row.get::<_, String>(2) == "group_user"
        })
        .map(|row| row.get(1))
        .collect();
    assert_eq!(settings_operations, vec!["ru".to_owned()]);
    for row in &rows {
        let model: String = row.get(0);
        let operation: String = row.get(1);
        let group: String = row.get(2);
        assert!(!["res.users.log", "ir.session", "ir.audit"].contains(&model.as_str()));
        assert!(["group_system", "group_user", "group_erp_manager"].contains(&group.as_str()));
        if group == "group_erp_manager" {
            assert!(["change.password.wizard", "change.password.user"].contains(&model.as_str()));
            assert_eq!(operation, "crud");
        }
        if group == "group_user" && model == "ir.attachment" {
            // the own attachments are readable (row filter) and the records the user may
            // write take attachments (create, write, delete)
            assert!(["r", "cud"].contains(&operation.as_str()), "{operation}");
            continue;
        }
        if group == "group_user" && model == "ir.filters" {
            // saved filters: read and create own or shared ones, write and delete own ones only
            assert!(["cr", "ud"].contains(&operation.as_str()), "{operation}");
            continue;
        }
        if group == "group_user" {
            // the Settings service and a user's own record (My Preferences)
            let expected = match model.as_str() {
                "ir.config.setting" | "res.users" => "ru",
                // a user's own settings row only (record domain on `user_id`; `user_id` itself is
                // writable by the system group alone)
                "change.password.own" | "res.users.identitycheck" | "res.users.settings" => "crud",
                _ => "r",
            };
            assert_eq!(operation, expected);
        }
    }
    assert!(
        rows.iter().any(
            |row| row.get::<_, String>(0) == "res.partner" && row.get::<_, String>(1) == "crud"
        )
    );
    // System Parameters: the system group alone, full access
    let parameter_rows: Vec<(String, String)> = rows
        .iter()
        .filter(|row| row.get::<_, String>(0) == "ir.config_parameter")
        .map(|row| (row.get(1), row.get(2)))
        .collect();
    assert_eq!(
        parameter_rows,
        [("crud".to_owned(), "group_system".to_owned())]
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM res_users WHERE active", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "base data must not seed an active default-password administrator"
    );

    let main_partner = client
        .query_one(
            "SELECT p.id, p.name FROM res_partner p JOIN ir_model_data d ON d.res_id=p.id \
             AND d.model='res.partner' WHERE d.module='base' AND d.name='main_partner'",
            &[],
        )
        .await
        .unwrap();
    let main_partner_id: i64 = main_partner.get(0);
    assert_eq!(main_partner.get::<_, String>(1), "My Company");

    let company_partner: i64 = client
        .query_one(
            "SELECT partner_id FROM res_company WHERE id=$1",
            &[&company_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        company_partner, main_partner_id,
        "main_company must link to main_partner"
    );

    let partner_seeds = client
        .query(
            "SELECT d.name, p.name, p.active, p.company_id FROM res_partner p JOIN ir_model_data d ON d.res_id=p.id \
             AND d.model='res.partner' WHERE d.module='base' AND d.name IN ('partner_root', 'partner_admin', 'public_partner') \
             ORDER BY d.name",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(partner_seeds.len(), 3);
    assert_eq!(partner_seeds[0].get::<_, String>(0), "partner_admin");
    assert_eq!(partner_seeds[0].get::<_, String>(1), "Administrator");
    assert!(partner_seeds[0].get::<_, bool>(2));
    assert_eq!(partner_seeds[0].get::<_, Option<i64>>(3), Some(company_id));
    assert_eq!(partner_seeds[1].get::<_, String>(0), "partner_root");
    assert_eq!(partner_seeds[1].get::<_, String>(1), "System");
    assert!(!partner_seeds[1].get::<_, bool>(2));
    assert_eq!(partner_seeds[1].get::<_, Option<i64>>(3), Some(company_id));
    assert_eq!(partner_seeds[2].get::<_, String>(0), "public_partner");
    assert_eq!(partner_seeds[2].get::<_, String>(1), "Public user");
    assert!(!partner_seeds[2].get::<_, bool>(2));
    assert_eq!(partner_seeds[2].get::<_, Option<i64>>(3), None);

    base_catalog::assert_language_catalog(&client).await;
    base_catalog::assert_currency_catalog(&client).await;
    let original_currency_ids = original_currency_ids::snapshot(&client).await;
    // Prove the observations depend on installed values, not only expected fixture bytes.
    let languages_before = base_catalog::language_catalog(&client).await;
    let currencies_before = base_catalog::currency_catalog(&client).await;
    client
        .execute(
            "UPDATE res_lang SET decimal_point=':' WHERE code='en_US'",
            &[],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE res_currency SET symbol='fixture-symbol' WHERE name='USD'",
            &[],
        )
        .await
        .unwrap();
    assert_ne!(
        base_catalog::language_catalog(&client).await,
        languages_before
    );
    assert_ne!(
        base_catalog::currency_catalog(&client).await,
        currencies_before
    );
    client
        .execute(
            "UPDATE res_lang SET decimal_point='.' WHERE code='en_US'",
            &[],
        )
        .await
        .unwrap();
    client
        .execute("UPDATE res_currency SET symbol='$' WHERE name='USD'", &[])
        .await
        .unwrap();
    base_catalog::assert_language_catalog(&client).await;
    base_catalog::assert_currency_catalog(&client).await;

    let currency_count: i64 = client
        .query_one("SELECT count(*) FROM res_currency", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        currency_count, 170,
        "res.currency catalog must have 170 currencies"
    );

    // PD-R2 / BUG-090: verify installed facts against SIX ISO4217, not upstream mistakes.
    let numeric = client.query("SELECT name, iso_numeric FROM res_currency WHERE name IN ('BYN','LTL','LVL','MRU','SLE','STN','UYW','VES','XCG','ZWG') ORDER BY name", &[]).await.unwrap();
    let numeric: Vec<(String, i32)> = numeric.iter().map(|row| (row.get(0), row.get(1))).collect();
    assert_eq!(
        numeric,
        vec![
            ("BYN".to_owned(), 933),
            ("LTL".to_owned(), 440),
            ("LVL".to_owned(), 428),
            ("MRU".to_owned(), 929),
            ("SLE".to_owned(), 925),
            ("STN".to_owned(), 930),
            ("UYW".to_owned(), 927),
            ("VES".to_owned(), 928),
            ("XCG".to_owned(), 532),
            ("ZWG".to_owned(), 924),
        ]
    );

    let indexed_rounding: erp_sdk::Decimal = client
        .query_one("SELECT rounding FROM res_currency WHERE name='UYI'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(indexed_rounding, erp_sdk::Decimal::ONE);
    let gold_code: String = client.query_one("SELECT c.name FROM res_currency c JOIN ir_model_data d ON d.res_id=c.id AND d.model='res.currency' WHERE d.module='base' AND d.name='ZIG'", &[]).await.unwrap().get(0);
    assert_eq!(
        gold_code, "ZWG",
        "historical external ID remains stable while ISO alpha code is corrected"
    );

    assert_eq!(
        client
            .query_one("SELECT count(*) FROM res_currency WHERE name='ZIG'", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let lang_count: i64 = client
        .query_one("SELECT count(*) FROM res_lang", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(lang_count, 93, "res.lang catalog must have 93 languages");
    let active_lang_count: i64 = client
        .query_one("SELECT count(*) FROM res_lang WHERE active", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        active_lang_count, 1,
        "only 1 language (en_US) should be active"
    );

    let email_size = client
        .query_one(
            "SELECT value FROM ir_config_parameter WHERE key='base.default_max_email_size'",
            &[],
        )
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(email_size, "20");

    let email_identity = client.query_one("SELECT d.res_id,d.noupdate FROM ir_model_data d JOIN ir_config_parameter p ON p.id=d.res_id WHERE d.module='base' AND d.model='ir.config_parameter' AND d.name='default_max_email_size' AND p.key='base.default_max_email_size'", &[]).await.unwrap();
    let email_id: i64 = email_identity.get(0);
    assert!(
        email_identity.get::<_, bool>(1),
        "email size seed must be noupdate"
    );
    client
        .execute(
            "UPDATE ir_config_parameter SET value='35' WHERE id=$1",
            &[&email_id],
        )
        .await
        .unwrap();

    let company_partner: Option<i64> = client
        .query_one(
            "SELECT partner_id FROM res_company WHERE id=$1",
            &[&company_id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        company_partner.is_some(),
        "company must have a partner_id linked"
    );
    let main_partner_id: i64 = client
        .query_one(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_partner'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        company_partner,
        Some(main_partner_id),
        "main_company must link to main_partner"
    );

    client
        .execute(
            "UPDATE res_company SET name='Renamed by operator' WHERE id=$1",
            &[&company_id],
        )
        .await
        .unwrap();

    // Tenant customization before upgrade: activate EUR with custom symbol and customize en_US date format.
    client
        .execute(
            "UPDATE res_currency SET active=true, symbol='E' WHERE name='EUR'",
            &[],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE res_lang SET date_format='%Y/%m/%d' WHERE code='en_US'",
            &[],
        )
        .await
        .unwrap();

    let upgraded = system
        .button_immediate_upgrade(
            &system.system_env(Context::new()),
            &[ModuleName::new("base").unwrap()],
        )
        .await
        .unwrap();
    assert!(upgraded.post_commit.failed.is_empty());
    let after = client.query(
        "SELECT name, res_id FROM ir_model_data WHERE module = 'base' AND name = ANY($1) ORDER BY name",
        &[&group_names.as_slice()]).await.unwrap();
    assert_eq!(
        identities,
        after
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect::<Vec<(String, i64)>>()
    );
    assert_eq!(
        client
            .query_one("SELECT name FROM res_company WHERE id=$1", &[&company_id])
            .await
            .unwrap()
            .get::<_, String>(0),
        "Renamed by operator"
    );

    // Operator edits must be preserved across module upgrade under noupdate semantics
    let eur = client
        .query_one(
            "SELECT active, symbol FROM res_currency WHERE name='EUR'",
            &[],
        )
        .await
        .unwrap();
    assert!(
        eur.get::<_, bool>(0),
        "EUR active status preserved across upgrade"
    );
    assert_eq!(
        eur.get::<_, String>(1),
        "E",
        "EUR custom symbol preserved across upgrade"
    );

    let lang_en = client
        .query_one("SELECT date_format FROM res_lang WHERE code='en_US'", &[])
        .await
        .unwrap();
    assert_eq!(
        lang_en.get::<_, String>(0),
        "%Y/%m/%d",
        "en_US date_format preserved across upgrade"
    );

    let currency_count_after: i64 = client
        .query_one("SELECT count(*) FROM res_currency", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        currency_count_after, 170,
        "upgrade must not duplicate currency rows"
    );
    let lang_count_after: i64 = client
        .query_one("SELECT count(*) FROM res_lang", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(lang_count_after, 93, "upgrade must not duplicate lang rows");
    assert_eq!(
        original_currency_ids::snapshot(&client).await,
        original_currency_ids
    );
    let email_after = client.query_one("SELECT p.id,p.value,d.noupdate FROM ir_config_parameter p JOIN ir_model_data d ON d.res_id=p.id AND d.model='ir.config_parameter' AND d.module='base' AND d.name='default_max_email_size' WHERE p.key='base.default_max_email_size'", &[]).await.unwrap();
    assert_eq!(email_after.get::<_, i64>(0), email_id);
    assert_eq!(email_after.get::<_, String>(1), "35");
    assert!(email_after.get::<_, bool>(2));
    let repeated = system
        .button_immediate_upgrade(
            &system.system_env(Context::new()),
            &[ModuleName::new("base").unwrap()],
        )
        .await
        .unwrap();
    assert!(repeated.post_commit.failed.is_empty());
    assert_eq!(
        original_currency_ids::snapshot(&client).await,
        original_currency_ids
    );
    drop(repeated);
    drop(client);
    drop(upgraded);
    drop(init);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn partner_identity_persists_and_requires_a_name() {
    use erp_sdk::{Cell, ErrorKind, RawVals};
    let harness = erp_testkit::Harness::builder([&erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = harness.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let error = tx
        .savepoint(async |tx| partners.create(tx, vec![RawVals::new()]).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The database refused the change: Contacts require a name"
    );
    let contact = partners
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Bootstrap Contact".into()))
                    .set_cell("email", Cell::Str("contact@example.invalid".into())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        contact.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(true)
    );
    assert_eq!(
        contact.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Null
    );
    let id = contact.rec_ids()[0].get();
    tx.commit().await.unwrap();
    let mut read = harness.begin().await.unwrap();
    let stored = read
        .env()
        .records("res.partner")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(
        stored.cell(&mut read, "email").await.unwrap(),
        Cell::Str("contact@example.invalid".into())
    );
    read.rollback().await.unwrap();
    harness.cleanup().await.unwrap();
}

/// What base grants `base.group_system` on a technical screen's model (`ir.access.csv`).
/// The web menu loader hides an entry whose action model the user cannot read, and the action
/// loader refuses it, so a screen without a read grant is not reachable by an administrator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AdminRead {
    /// Read (and possibly more) granted before revision r3; the test checks the screen opens.
    Granted,
    /// Read only, granted in revision r3 (lead decision 2026-10-25): the screen opens, and the
    /// test checks that create, write and unlink are refused.
    ReadOnly,
    /// Full access for the system group, granted: `ir.config_parameter`,
    /// whose secret keys stay hidden (probed separately).
    Managed,
}

/// One Technical screen of `addons/base/data/technical_views.xml`: menu, its category, window
/// action, model and the list, form and search views (all xml ids in module `base`).
struct Screen {
    menu: &'static str,
    category: &'static str,
    action: &'static str,
    model: &'static str,
    views: &'static str,
    admin: AdminRead,
}

const TECHNICAL_CATEGORIES: [&str; 8] = [
    "technical_db_menu",
    "technical_seq_menu",
    "technical_ui_menu",
    "technical_actions_menu",
    "technical_params_menu",
    "technical_security_menu",
    "technical_modules_menu",
    "technical_automation_menu",
];

const TECHNICAL_SCREENS: [Screen; 20] = {
    use AdminRead::{Granted, Managed, ReadOnly};
    const fn s(
        menu: &'static str,
        category: &'static str,
        action: &'static str,
        model: &'static str,
        views: &'static str,
        admin: AdminRead,
    ) -> Screen {
        Screen {
            menu,
            category,
            action,
            model,
            views,
            admin,
        }
    }
    [
        s(
            "technical_models_menu",
            "technical_db_menu",
            "technical_model_action",
            "ir.model",
            "technical_model",
            Granted,
        ),
        s(
            "technical_fields_menu",
            "technical_db_menu",
            "technical_fields_action",
            "ir.model.fields",
            "technical_fields",
            Granted,
        ),
        s(
            "technical_relations_menu",
            "technical_db_menu",
            "technical_relation_action",
            "ir.model.relation",
            "technical_relation",
            ReadOnly,
        ),
        s(
            "technical_precision_menu",
            "technical_db_menu",
            "technical_precision_action",
            "decimal.precision",
            "technical_precision",
            ReadOnly,
        ),
        s(
            "technical_sequences_menu",
            "technical_seq_menu",
            "technical_sequence_action",
            "ir.sequence",
            "technical_sequence",
            ReadOnly,
        ),
        s(
            "technical_seq_ranges_menu",
            "technical_seq_menu",
            "technical_seq_range_action",
            "ir.sequence.date_range",
            "technical_seq_range",
            ReadOnly,
        ),
        s(
            "technical_data_menu",
            "technical_seq_menu",
            "technical_model_data_action",
            "ir.model.data",
            "technical_model_data",
            ReadOnly,
        ),
        s(
            "technical_views_menu",
            "technical_ui_menu",
            "technical_view_action",
            "ir.ui.view",
            "technical_view",
            Granted,
        ),
        s(
            "technical_menus_menu",
            "technical_ui_menu",
            "technical_menu_action",
            "ir.ui.menu",
            "technical_menu",
            Granted,
        ),
        s(
            "technical_actions_act_menu",
            "technical_actions_menu",
            "technical_actions_action",
            "ir.actions.actions",
            "technical_actions",
            ReadOnly,
        ),
        s(
            "technical_act_window_menu",
            "technical_actions_menu",
            "technical_act_window_action",
            "ir.actions.act_window",
            "technical_act_window",
            Granted,
        ),
        s(
            "technical_act_server_menu",
            "technical_actions_menu",
            "technical_act_server_action",
            "ir.actions.server",
            "technical_act_server",
            ReadOnly,
        ),
        s(
            "technical_act_client_menu",
            "technical_actions_menu",
            "technical_act_client_action",
            "ir.actions.client",
            "technical_act_client",
            ReadOnly,
        ),
        s(
            "technical_act_url_menu",
            "technical_actions_menu",
            "technical_act_url_action",
            "ir.actions.act_url",
            "technical_act_url",
            ReadOnly,
        ),
        s(
            "technical_defaults_menu",
            "technical_actions_menu",
            "technical_default_action",
            "ir.default",
            "technical_default",
            ReadOnly,
        ),
        s(
            "technical_params_act_menu",
            "technical_params_menu",
            "technical_config_param_action",
            "ir.config_parameter",
            "technical_config_param",
            Managed,
        ),
        s(
            "technical_privilege_menu",
            "technical_security_menu",
            "technical_privilege_action",
            "res.groups.privilege",
            "technical_privilege",
            Granted,
        ),
        s(
            "technical_modules_act_menu",
            "technical_modules_menu",
            "technical_module_action",
            "ir.module.module",
            "technical_module",
            Granted,
        ),
        s(
            "technical_jobs_menu",
            "technical_automation_menu",
            "technical_job_action",
            "ir.job",
            "technical_job",
            Granted,
        ),
        s(
            "technical_job_runs_menu",
            "technical_automation_menu",
            "technical_job_run_action",
            "ir.job.run",
            "technical_job_run",
            Granted,
        ),
    ]
};

/// The Access Rules entry reuses the Settings action of `settings_views.xml` (no own views).
const TECHNICAL_ACCESS_MENU: (&str, &str, &str) = (
    "technical_access_menu",
    "technical_security_menu",
    "settings_access_action",
);

/// The Background Job Settings entry opens the Settings service page on the jobs settings
/// (menu, category, action, form view; no list or search view).
const TECHNICAL_JOBS_SETTINGS: (&str, &str, &str, &str) = (
    "technical_jobs_settings_menu",
    "technical_automation_menu",
    "technical_jobs_settings_action",
    "technical_jobs_settings_form",
);

async fn base_xmlid(client: &tokio_postgres::Client, model: &str, name: &str) -> i64 {
    let rows = client
        .query(
            "SELECT res_id::bigint FROM ir_model_data WHERE module = 'base' AND model = $1 AND name = $2",
            &[&model, &name],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "base.{name} ({model}) must be loaded exactly once"
    );
    rows[0].get(0)
}

/// The base xml ids (`group_system`, ...) of a relation's groups, sorted.
async fn group_xmlids(client: &tokio_postgres::Client, sql: &str, id: i64) -> Vec<String> {
    client
        .query(sql, &[&id])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect()
}

const MENU_GROUPS_SQL: &str = "SELECT d.name FROM ir_ui_menu_group_rel r JOIN ir_model_data d \
     ON d.model = 'res.groups' AND d.res_id = r.gid AND d.module = 'base' \
     WHERE r.menu_id = $1 ORDER BY d.name";
const ACTION_GROUPS_SQL: &str = "SELECT d.name FROM ir_act_window_group_rel r JOIN ir_model_data d \
     ON d.model = 'res.groups' AND d.res_id = r.gid AND d.module = 'base' \
     WHERE r.act_id = $1 ORDER BY d.name";

/// Menu parent id, action reference and group xml ids, as stored.
async fn menu_row(
    client: &tokio_postgres::Client,
    id: i64,
) -> (Option<i64>, Option<String>, Vec<String>) {
    let row = client
        .query_one(
            "SELECT parent_id::bigint, action FROM ir_ui_menu WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    (
        row.get(0),
        row.get(1),
        group_xmlids(client, MENU_GROUPS_SQL, id).await,
    )
}

fn model_denial(verb: &str, description: &str, model: &str, groups: &[&str]) -> String {
    let group_info = if groups.is_empty() {
        "No group may do this.".to_owned()
    } else {
        let list: Vec<String> = groups.iter().map(|g| format!("\t- {g}")).collect();
        format!("Groups that may do this:\n{}", list.join("\n"))
    };
    format!(
        "Your access rights do not let you {verb} '{description}' ({model}) records.\n\n{group_info}\n\n\
         Ask an administrator if you need this access."
    )
}

/// The value of the provider credential the Technical test stores (never in a response).
const PROVIDER_SECRET: &str = "provider-sentinel-7f3a-credential";
/// The secret rows the Technical test probes: two created by `init`, one provider credential.
const SECRET_PROBE_KEYS: [&str; 3] = ["database.secret", "database.uuid", "recaptcha_private_key"];

/// The administrator never sees a secret parameter by any read path, replaces one without reading
/// it, and is refused the identity keys; the internal user cannot replace one (
async fn secret_parameter_probes(
    call: &impl AsyncFn(String, serde_json::Value, &erp_orm::Principal) -> (u16, serde_json::Value),
    admin: &erp_orm::Principal,
    ordinary: &erp_orm::Principal,
    secrets: &[(String, i64, String)],
    orm: &erp_orm::Orm,
) {
    use serde_json::json;
    let keys: Vec<&str> = secrets.iter().map(|(k, _, _)| k.as_str()).collect();
    let ids: Vec<i64> = secrets.iter().map(|(_, id, _)| *id).collect();
    let values: Vec<&str> = secrets.iter().map(|(_, _, v)| v.as_str()).collect();
    let path = |method: &str| format!("/json/2/ir.config_parameter/{method}");
    // nothing a response carries may name a secret key or hold a secret value
    let leaks = |body: &serde_json::Value| -> Vec<String> {
        let text = body.to_string();
        keys.iter()
            .chain(values.iter())
            .filter(|needle| text.contains(**needle))
            .map(|needle| (*needle).to_owned())
            .collect()
    };
    let mut searches = vec![
        (
            "search_read",
            json!({"domain": [], "fields": ["key", "value"]}),
        ),
        (
            "web_search_read",
            json!({"domain": [], "specification": {"key": {}, "value": {}, "display_name": {}}}),
        ),
        ("name_search", json!({"name": "", "limit": 1000})),
        ("name_search", json!({"name": "recaptcha", "limit": 1000})),
        (
            "formatted_read_group",
            json!({"domain": [], "groupby": ["key"], "aggregates": ["__count"]}),
        ),
    ];
    let mut empty_domains = vec![json!([["key", "in", keys]])];
    for value in &values {
        empty_domains.push(json!([["value", "=", value]]));
        empty_domains.push(json!([["value", "ilike", &value[..value.len() / 2]]]));
    }
    for domain in &empty_domains {
        searches.push(("search_read", json!({"domain": domain, "fields": ["key"]})));
        searches.push((
            "web_search_read",
            json!({"domain": domain, "specification": {"key": {}}}),
        ));
    }
    for (method, body) in searches {
        let (status, rows) = call(path(method), body.clone(), admin).await;
        assert_eq!(status, 200, "{method} {body}: {rows}");
        assert!(
            leaks(&rows).is_empty(),
            "{method} {body} leaks {:?}",
            leaks(&rows)
        );
    }
    for domain in &empty_domains {
        let (status, n) = call(path("search_count"), json!({"domain": domain}), admin).await;
        assert_eq!(
            (status, n.clone()),
            (200, json!(0)),
            "search_count {domain}"
        );
        let (_, rows) = call(
            path("search_read"),
            json!({"domain": domain, "fields": ["key"]}),
            admin,
        )
        .await;
        assert_eq!(
            rows,
            json!([]),
            "a domain matching a secret returns nothing: {domain}"
        );
    }
    // reading a secret's id or its value by key is refused
    for (method, body) in [
        ("read", json!({"ids": ids, "fields": ["key", "value"]})),
        (
            "web_read",
            json!({"ids": ids, "specification": {"key": {}, "value": {}}}),
        ),
    ] {
        let (status, error) = call(path(method), body, admin).await;
        assert_eq!(status, 403, "{method}: {error}");
        assert_eq!(error["name"], "erp.exceptions.AccessError", "{method}");
        assert!(
            leaks(&error).iter().all(|l| keys.contains(&l.as_str())),
            "{method} {error}"
        );
    }
    for key in &keys {
        let (status, error) = call(
            path("get_str"),
            json!({"key": key, "default": "absent"}),
            admin,
        )
        .await;
        assert_eq!(status, 403, "get_str {key}: {error}");
        assert_eq!(error["name"], "erp.exceptions.AccessError", "get_str {key}");
    }
    // a provider credential is replaced, never read back; plain writes onto a secret key refused
    let replaced = "provider-replaced-91c2-credential";
    let refusal = "ir.config_parameter recaptcha_private_key is a secret: its value is never \
                   returned; replace it with erp_replace_secret.";
    let (status, created) = call(
        path("create"),
        json!({"vals_list": [{"key": "integrator.note", "value": "plain"}]}),
        admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "an administrator creates a parameter: {created}"
    );
    let note = created[0]
        .as_i64()
        .unwrap_or_else(|| panic!("create: {created}"));
    for (method, body) in [
        (
            "set_str",
            json!({"key": "recaptcha_private_key", "value": replaced}),
        ),
        (
            "create",
            json!({"vals_list": [{"key": "recaptcha_private_key", "value": replaced}]}),
        ),
        (
            "write",
            json!({"ids": [note], "vals": {"key": "recaptcha_private_key"}}),
        ),
    ] {
        let (status, error) = call(path(method), body, admin).await;
        assert_eq!(status, 403, "{method}: {error}");
        assert_eq!(
            (&error["name"], &error["message"]),
            (&json!("erp.exceptions.AccessError"), &json!(refusal)),
            "{method}"
        );
    }
    // a key or value from a context default would skip every key check: refused first
    for context in [
        json!({"default_key": "recaptcha_private_key"}),
        json!({"default_value": "file"}),
    ] {
        let (status, error) = call(
            path("create"),
            json!({"vals_list": [{"key": "ir_attachment.location", "value": "x"}], "context": context}),
            admin,
        )
        .await;
        assert_eq!(status, 422, "{context}: {error}");
        assert_eq!(
            (&error["name"], &error["message"]),
            (
                &json!("erp.exceptions.ValidationError"),
                &json!(
                    "A system parameter takes its key and value from the record, never from a \
                     default in the context."
                )
            ),
            "{context}"
        );
    }
    let (status, done) = call(
        path("erp_replace_secret"),
        json!({"key": "recaptcha_private_key", "value": replaced}),
        admin,
    )
    .await;
    assert_eq!(status, 200, "erp_replace_secret: {done}");
    assert!(!done.to_string().contains(PROVIDER_SECRET), "{done}");
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let stored: String = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key='recaptcha_private_key'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    assert_eq!(stored, replaced);
    let (_, rows) = call(
        path("search_read"),
        json!({"domain": [["value", "=", replaced]], "fields": ["key"]}),
        admin,
    )
    .await;
    assert_eq!(rows, json!([]), "the replaced value is not searchable");
    for (key, name, message) in [
        (
            "database.secret",
            "erp.exceptions.AccessError",
            "Parameter database.secret is created with the database and cannot be replaced over \
             RPC (rotate it with erp db rotate-secret).",
        ),
        (
            "database.uuid",
            "erp.exceptions.AccessError",
            "Parameter database.uuid is created with the database and cannot be replaced over \
             RPC.",
        ),
        (
            "integrator.note",
            "erp.exceptions.ValidationError",
            "Parameter integrator.note is not a secret; change it in System Parameters.",
        ),
    ] {
        let (_, error) = call(
            path("erp_replace_secret"),
            json!({"key": key, "value": "x".repeat(40)}),
            admin,
        )
        .await;
        assert_eq!(
            (&error["name"], &error["message"]),
            (&json!(name), &json!(message)),
            "erp_replace_secret {key}"
        );
    }
    let (status, error) = call(
        path("erp_replace_secret"),
        json!({"key": "recaptcha_private_key", "value": "internal-user-try"}),
        ordinary,
    )
    .await;
    assert_eq!(
        status, 403,
        "an internal user cannot replace a secret: {error}"
    );
    assert_eq!(error["name"], "erp.exceptions.AccessError");
    // protected keys: a declared key and a default key are neither deleted nor renamed
    let protected = [
        "base.partition_upkeep_interval_secs",
        "base.login_cooldown_after",
    ];
    let (_, rows) = call(
        path("search_read"),
        json!({"domain": [["key", "in", protected]], "fields": ["key"], "order": "key"}),
        admin,
    )
    .await;
    let rows = rows
        .as_array()
        .unwrap_or_else(|| panic!("protected rows: {rows}"));
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in rows {
        let (id, key) = (row["id"].clone(), row["key"].as_str().unwrap().to_owned());
        let (_, error) = call(path("unlink"), json!({"ids": [id]}), admin).await;
        assert_eq!(
            error["message"],
            json!(format!("You cannot delete the {key} record.")),
            "unlink {key}"
        );
        let (_, error) = call(
            path("write"),
            json!({"ids": [id], "vals": {"key": "integrator.renamed"}}),
            admin,
        )
        .await;
        assert_eq!(
            error["message"],
            json!(format!(
                "You cannot rename config parameters with keys {key}"
            )),
            "rename {key}"
        );
    }
}

/// Ids the read-only probes refer to.
struct ProbeIds {
    partner_model: i64,
    partner_email_field: i64,
    sequence: i64,
}

/// For a read-only technical model: valid `create` values and `write` values, so a refusal
/// comes from access, not from a malformed request.
fn write_probe(model: &str, ids: &ProbeIds) -> (serde_json::Value, serde_json::Value) {
    use serde_json::json;
    let renamed = json!({"name": "Renamed by a read-only administrator"});
    match model {
        "ir.model.relation" => (
            json!({"name": "x_refused_rel", "model_id": ids.partner_model}),
            renamed,
        ),
        "decimal.precision" => (json!({"name": "Refused precision", "digits": 3}), renamed),
        "ir.sequence" => (json!({"name": "Refused sequence"}), renamed),
        "ir.sequence.date_range" => (
            json!({"sequence_id": ids.sequence, "date_from": "2027-01-01", "date_to": "2027-12-31"}),
            json!({"number_next": 42}),
        ),
        "ir.model.data" => (
            json!({"module": "technical_test", "name": "refused", "model": "res.partner"}),
            json!({"noupdate": true}),
        ),
        "ir.actions.actions" => (
            json!({"name": "Refused action", "type": "ir.actions.act_url"}),
            renamed,
        ),
        "ir.actions.server" => (
            json!({"name": "Refused server action", "state": "code", "model_id": ids.partner_model}),
            renamed,
        ),
        "ir.actions.client" => (
            json!({"name": "Refused client action", "tag": "technical_refused"}),
            renamed,
        ),
        "ir.actions.act_url" => (
            json!({"name": "Refused URL action", "url": "/technical-refused"}),
            renamed,
        ),
        "ir.default" => (
            json!({"field_id": ids.partner_email_field, "json_value": "\"refused@example.invalid\""}),
            json!({"condition": "refused"}),
        ),
        other => panic!("no write probe for read-only model {other}"),
    }
}

#[test]
fn real_base_manifest_initializes_technical_menu_and_views() {
    // The module install plus native RPC chain needs more than the default test-thread stack
    // (same reason as `human_metadata.rs`).
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(technical_menu_and_views());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end walk over the Technical table"
)]
async fn technical_menu_and_views() {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use erp_fields::{Cell, RecId};
    use erp_orm::{Command, Orm, RawVals, SUPERUSER_ID};
    use erp_rpc::{Caller, RpcCall, RpcConfig, RpcService};
    use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
    use erp_security::{resolver::Resolver, source::PolicySource};

    struct Host;
    impl erp_rpc::RpcHost for Host {
        fn language_active(&self, code: &str) -> erp_fields::Result<bool> {
            Ok(code == "en_US")
        }
        fn has_capability(
            &self,
            _: &erp_orm::Principal,
            _: &erp_orm::Capability,
        ) -> erp_fields::Result<bool> {
            Ok(false)
        }
    }

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
    // Explicit provenance for this binary's private native fixture bytes.
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
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(native_artifact_identities)
        .code([&erp_addon_base::ADDON, &erp_addon_web::ADDON])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = system.initialize(InitOptions::new()).await.unwrap();
    // a database that serves audited calls carries the audit table; the
    // publication step installs it in a real deployment, this fixture does it directly.
    {
        let mut audit_tx = init
            .orm
            .begin_opts(
                erp_orm::BeginOptions::new(SUPERUSER_ID)
                    .isolation(erp_orm::Isolation::ReadCommitted)
                    .on_primary(),
            )
            .await
            .unwrap();
        audit_tx.install_audit_schema().await.unwrap();
        audit_tx.commit().await.unwrap();
    }
    assert!(init.data_notices.is_empty(), "{:?}", init.data_notices);
    assert!(init.post_commit.failed.is_empty());
    let client = db.client().await.unwrap();

    // 1. The loaded data: every technical xml id is in the table below, and each stored value
    //    (parent, action, groups, model, views) is read back from the database.
    let mut expected_ids: BTreeSet<String> = BTreeSet::new();
    expected_ids.insert("technical_menu".to_owned());
    expected_ids.extend(TECHNICAL_CATEGORIES.iter().map(|id| (*id).to_owned()));
    expected_ids.insert(TECHNICAL_ACCESS_MENU.0.to_owned());
    let (jobs_menu, jobs_category, jobs_action, jobs_form) = TECHNICAL_JOBS_SETTINGS;
    expected_ids.extend([jobs_menu, jobs_action, jobs_form].map(str::to_owned));
    for screen in &TECHNICAL_SCREENS {
        expected_ids.insert(screen.menu.to_owned());
        expected_ids.insert(screen.action.to_owned());
        for suffix in ["list", "form", "search"] {
            expected_ids.insert(format!("{}_{suffix}", screen.views));
            expected_ids.insert(format!("{}_action_{suffix}", screen.views));
        }
        // a search view is wired by `search_view_id`, not by an act_window.view row
        expected_ids.remove(&format!("{}_action_search", screen.views));
    }
    let loaded: BTreeSet<String> = client
        .query(
            "SELECT name FROM ir_model_data WHERE module = 'base' AND name LIKE 'technical\\_%'",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        loaded, expected_ids,
        "technical_views.xml ids and the test table differ"
    );

    let settings_menu = base_xmlid(&client, "ir.ui.menu", "settings_menu").await;
    let technical_menu = base_xmlid(&client, "ir.ui.menu", "technical_menu").await;
    let (parent, action, groups) = menu_row(&client, technical_menu).await;
    assert_eq!(parent, Some(settings_menu));
    assert_eq!(action, None);
    assert_eq!(groups, ["group_system"]);
    let mut category_ids = BTreeMap::new();
    for category in TECHNICAL_CATEGORIES {
        let id = base_xmlid(&client, "ir.ui.menu", category).await;
        let (parent, action, groups) = menu_row(&client, id).await;
        assert_eq!(parent, Some(technical_menu), "{category} parent");
        assert_eq!(action, None, "{category} is a category without action");
        assert!(
            groups.is_empty(),
            "{category} inherits the Technical menu's group"
        );
        category_ids.insert(category, id);
    }
    let (access_menu, access_category, access_action) = TECHNICAL_ACCESS_MENU;
    let access_menu_id = base_xmlid(&client, "ir.ui.menu", access_menu).await;
    let access_action_id = base_xmlid(&client, "ir.actions.act_window", access_action).await;
    let (parent, action, groups) = menu_row(&client, access_menu_id).await;
    assert_eq!(parent, Some(category_ids[access_category]));
    assert_eq!(
        action,
        Some(format!("ir.actions.act_window,{access_action_id}"))
    );
    assert_eq!(groups, ["group_system"]);
    let jobs_menu_id = base_xmlid(&client, "ir.ui.menu", jobs_menu).await;
    let jobs_action_id = base_xmlid(&client, "ir.actions.act_window", jobs_action).await;
    let jobs_form_id = base_xmlid(&client, "ir.ui.view", jobs_form).await;
    let (parent, action, groups) = menu_row(&client, jobs_menu_id).await;
    assert_eq!(parent, Some(category_ids[jobs_category]));
    assert_eq!(
        action,
        Some(format!("ir.actions.act_window,{jobs_action_id}"))
    );
    assert_eq!(groups, ["group_system"]);
    let row = client
        .query_one(
            "SELECT a.res_model, a.view_mode, a.view_id::bigint, v.model, v.type \
             FROM ir_act_window a JOIN ir_ui_view v ON v.id = a.view_id WHERE a.id = $1",
            &[&jobs_action_id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "ir.config.setting");
    assert_eq!(row.get::<_, String>(1), "form");
    assert_eq!(row.get::<_, Option<i64>>(2), Some(jobs_form_id));
    assert_eq!(row.get::<_, String>(3), "ir.config.setting");
    assert_eq!(row.get::<_, String>(4), "form");

    struct Loaded {
        menu: i64,
        action: i64,
        list: i64,
        form: i64,
    }
    let mut loaded_screens = Vec::new();
    for screen in &TECHNICAL_SCREENS {
        let menu = base_xmlid(&client, "ir.ui.menu", screen.menu).await;
        let action = base_xmlid(&client, "ir.actions.act_window", screen.action).await;
        let (parent, menu_action, groups) = menu_row(&client, menu).await;
        assert_eq!(
            parent,
            Some(category_ids[screen.category]),
            "{} parent",
            screen.menu
        );
        assert_eq!(
            menu_action,
            Some(format!("ir.actions.act_window,{action}")),
            "{} action",
            screen.menu
        );
        assert_eq!(groups, ["group_system"], "{} groups", screen.menu);

        let mut view_ids = BTreeMap::new();
        for view_type in ["list", "form", "search"] {
            let id = base_xmlid(
                &client,
                "ir.ui.view",
                &format!("{}_{view_type}", screen.views),
            )
            .await;
            let row = client
                .query_one(
                    "SELECT model, type, mode, active FROM ir_ui_view WHERE id = $1",
                    &[&id],
                )
                .await
                .unwrap();
            assert_eq!(
                row.get::<_, String>(0),
                screen.model,
                "{} {view_type} model",
                screen.views
            );
            assert_eq!(
                row.get::<_, String>(1),
                view_type,
                "{} {view_type} type",
                screen.views
            );
            assert_eq!(row.get::<_, String>(2), "primary");
            assert!(row.get::<_, bool>(3));
            view_ids.insert(view_type, id);
        }
        let row = client
            .query_one(
                "SELECT res_model, view_mode, view_id::bigint, search_view_id::bigint, target \
                 FROM ir_act_window WHERE id = $1",
                &[&action],
            )
            .await
            .unwrap();
        assert_eq!(
            row.get::<_, String>(0),
            screen.model,
            "{} res_model",
            screen.action
        );
        assert_eq!(row.get::<_, String>(1), "list,form");
        assert_eq!(row.get::<_, Option<i64>>(2), Some(view_ids["list"]));
        assert_eq!(
            row.get::<_, Option<i64>>(3),
            Some(view_ids["search"]),
            "{} search view",
            screen.action
        );
        assert_eq!(row.get::<_, String>(4), "current");
        assert_eq!(
            group_xmlids(&client, ACTION_GROUPS_SQL, action).await,
            ["group_system"],
            "{} groups",
            screen.action
        );
        let modes: BTreeSet<(String, i64)> = client
            .query(
                "SELECT view_mode, view_id::bigint FROM ir_act_window_view WHERE act_window_id = $1",
                &[&action],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        assert_eq!(
            modes,
            BTreeSet::from([
                ("form".to_owned(), view_ids["form"]),
                ("list".to_owned(), view_ids["list"]),
            ]),
            "{} views",
            screen.action
        );
        loaded_screens.push(Loaded {
            menu,
            action,
            list: view_ids["list"],
            form: view_ids["form"],
        });
    }
    let main_company = base_xmlid(&client, "res.company", "main_company").await;
    let system_group = base_xmlid(&client, "res.groups", "group_system").await;
    let user_group = base_xmlid(&client, "res.groups", "group_user").await;
    let partner_model: i64 = client
        .query_one(
            "SELECT id::bigint FROM ir_model WHERE model = 'res.partner'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let partner_email_field: i64 = client
        .query_one(
            "SELECT id::bigint FROM ir_model_fields WHERE model = 'res.partner' AND name = 'email'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    drop(client);

    // 2. The same screens through the native RPC stack, as the browser client calls them.
    let source = PolicySource::build(Arc::clone(init.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let resolver = Resolver::for_registry(init.orm.registry()).unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(init.orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(init.orm.registry()),
            init.orm
                .env(SUPERUSER_ID, erp_orm::Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(resolver)
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let auth = Auth::new(
        orm.clone(),
        erp_security::auth::AuthConfig::DEFAULT,
        HashPool::global(),
    )
    .await
    .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    for (login, group) in [
        ("technical-admin", system_group),
        ("technical-user", user_group),
    ] {
        let partner = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(login.into()))
                        .set_cell("company_id", Cell::Id(main_company)),
                ],
            )
            .await
            .unwrap();
        tx.env()
            .records("res.users")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(login.into()))
                        .set_cell("login", Cell::Str(login.into()))
                        .set_cell("password", Cell::Str(format!("{login}-password").into()))
                        .set_cell("partner_id", Cell::Id(partner.ids()[0]))
                        .set_cell("company_id", Cell::Id(main_company))
                        .set_cell("lang", Cell::Str("en_US".into()))
                        .set_cell("tz", Cell::Str("UTC".into()))
                        .set_commands(
                            "company_ids",
                            [Command::set([RecId::from_real(main_company).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::set([RecId::from_real(group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
    }
    // One record of each read-only model that base data may leave empty, so write and unlink
    // are probed on a real row (created as the superuser, outside the ACL under test).
    let create = async |tx: &mut erp_orm::Transaction, model: &str, vals: RawVals| -> i64 {
        tx.env()
            .records(model)
            .unwrap()
            .create(tx, vec![vals])
            .await
            .unwrap()
            .ids()[0]
    };
    create(
        &mut tx,
        "decimal.precision",
        RawVals::new()
            .set_cell("name", Cell::Str("Technical Test Precision".into()))
            .set_cell("digits", Cell::Int(3)),
    )
    .await;
    let sequence = create(
        &mut tx,
        "ir.sequence",
        RawVals::new()
            .set_cell("name", Cell::Str("Technical Test Sequence".into()))
            .set_cell("code", Cell::Str("technical.test".into())),
    )
    .await;
    create(
        &mut tx,
        "ir.sequence.date_range",
        RawVals::new()
            .set_cell("sequence_id", Cell::Id(sequence))
            .set_cell(
                "date_from",
                Cell::Date(erp_fields::Date::from_ymd(2026, 1, 1).unwrap()),
            )
            .set_cell(
                "date_to",
                Cell::Date(erp_fields::Date::from_ymd(2026, 12, 31).unwrap()),
            ),
    )
    .await;
    create(
        &mut tx,
        "ir.actions.server",
        RawVals::new()
            .set_cell("name", Cell::Str("Technical Test Server Action".into()))
            .set_cell("state", Cell::Str("code".into()))
            .set_cell("model_id", Cell::Id(partner_model)),
    )
    .await;
    create(
        &mut tx,
        "ir.actions.client",
        RawVals::new()
            .set_cell("name", Cell::Str("Technical Test Client Action".into()))
            .set_cell("tag", Cell::Str("technical_test".into())),
    )
    .await;
    create(
        &mut tx,
        "ir.actions.act_url",
        RawVals::new()
            .set_cell("name", Cell::Str("Technical Test URL Action".into()))
            .set_cell("url", Cell::Str("/technical-test".into())),
    )
    .await;
    create(
        &mut tx,
        "ir.default",
        RawVals::new()
            .set_cell("field_id", Cell::Id(partner_email_field))
            .set_cell(
                "json_value",
                Cell::Str("\"default@example.invalid\"".into()),
            ),
    )
    .await;
    // a provider credential of the closed secret list
    create(
        &mut tx,
        "ir.config_parameter",
        RawVals::new()
            .set_cell("key", Cell::Str("recaptcha_private_key".into()))
            .set_cell("value", Cell::Str(PROVIDER_SECRET.into())),
    )
    .await;
    let mut secrets: Vec<(String, i64, String)> = Vec::new();
    for key in SECRET_PROBE_KEYS {
        let row = tx
            .query(
                "SELECT id::int8, value FROM ir_config_parameter WHERE key = $1",
                &[&key],
            )
            .await
            .unwrap();
        secrets.push((key.to_owned(), row[0].get(0), row[0].get(1)));
    }
    tx.commit().await.unwrap();
    let probe_ids = ProbeIds {
        partner_model,
        partner_email_field,
        sequence,
    };
    let model_label = |model: &str| -> String {
        let spec = init.orm.registry().model(model).unwrap();
        assert!(!spec.description.is_empty(), "{model} has a description");
        spec.description.to_string()
    };
    let principal = async |login: &str| {
        let logged = auth
            .authenticate(LoginRequest::new(
                login,
                format!("{login}-password"),
                ClientAddr::InProcess("technical-menu-test"),
            ))
            .await
            .unwrap();
        let mut begun = auth
            .begin_session(logged.session().secret(), CredentialOptions::new())
            .await
            .unwrap();
        let principal = begun.tx.env().principal().clone();
        begun.tx.rollback().await.unwrap();
        principal
    };
    let admin = principal("technical-admin").await;
    let ordinary = principal("technical-user").await;
    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
    let call = async |path: String, body: serde_json::Value, who: &erp_orm::Principal| {
        let response = service
            .call(RpcCall::json2(
                &path,
                serde_json::to_vec(&body).unwrap(),
                Caller::Principal(who.clone()),
            ))
            .await;
        let json: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        (response.status(), json)
    };
    let menu_nodes = |tree: &serde_json::Value| -> BTreeMap<i64, serde_json::Value> {
        tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| (node["id"].as_i64().unwrap(), node.clone()))
            .collect()
    };

    let (status, tree) = call(
        "/json/2/ir.ui.menu/erp_load_web_menus".to_owned(),
        serde_json::json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{tree}");
    let admin_menus = menu_nodes(&tree);
    assert_eq!(admin_menus[&technical_menu]["parent_id"], settings_menu);
    // every category has a reachable entry for the administrator, Parameters included
    for (category, id) in &category_ids {
        assert_eq!(admin_menus[id]["parent_id"], technical_menu, "{category}");
    }
    assert_eq!(admin_menus[&access_menu_id]["action_id"], access_action_id);

    for (screen, ids) in TECHNICAL_SCREENS.iter().zip(&loaded_screens) {
        let (status, action) = call(
            "/json/2/ir.actions.act_window/erp_load_web_action".to_owned(),
            serde_json::json!({"ids": [ids.action]}),
            &admin,
        )
        .await;
        match screen.admin {
            AdminRead::Granted | AdminRead::ReadOnly | AdminRead::Managed => {
                let node = &admin_menus[&ids.menu];
                assert_eq!(node["action_id"], ids.action, "{}", screen.menu);
                assert_eq!(
                    node["parent_id"], category_ids[screen.category],
                    "{}",
                    screen.menu
                );
                assert_eq!(status, 200, "{}: {action}", screen.action);
                assert_eq!(action["model"], screen.model);
                let views: Vec<(i64, String)> = action["views"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|view| {
                        (
                            view["id"].as_i64().unwrap_or_else(|| {
                                panic!("{}: action view id: {action}", screen.action)
                            }),
                            view["type"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect();
                assert_eq!(
                    views,
                    [(ids.list, "list".to_owned()), (ids.form, "form".to_owned())],
                    "{}",
                    screen.action
                );
                let (status, description) = call(
                    format!("/json/2/{}/get_views", screen.model),
                    serde_json::json!({
                        "views": [[ids.list, "list"], [ids.form, "form"]],
                        "wire_major": 1,
                        "options": {"mobile": false},
                    }),
                    &admin,
                )
                .await;
                assert_eq!(status, 200, "{} get_views: {description}", screen.model);
                assert!(description["notices"].as_array().unwrap().is_empty());
                let identities: Vec<i64> = description["views"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|view| {
                        view["identity"]["id"].as_i64().unwrap_or_else(|| {
                            panic!("{} get_views identity: {description}", screen.model)
                        })
                    })
                    .collect();
                assert_eq!(identities, [ids.list, ids.form], "{}", screen.model);
                let specification: serde_json::Map<String, serde_json::Value> =
                    description["fields"][screen.model]
                        .as_object()
                        .unwrap()
                        .iter()
                        .map(|(name, field)| {
                            let spec = if field["relation"].is_null() {
                                serde_json::json!({})
                            } else {
                                serde_json::json!({"fields": {"display_name": {}}})
                            };
                            (name.clone(), spec)
                        })
                        .collect();
                let (status, rows) = call(
                    format!("/json/2/{}/web_search_read", screen.model),
                    serde_json::json!({
                        "domain": [],
                        "specification": specification,
                        "offset": 0,
                        "limit": 2,
                        "order": "id asc",
                    }),
                    &admin,
                )
                .await;
                assert_eq!(status, 200, "{} web_search_read: {rows}", screen.model);
                if screen.admin == AdminRead::ReadOnly {
                    let record = rows["records"][0]["id"]
                        .as_i64()
                        .unwrap_or_else(|| panic!("{}: a record to probe: {rows}", screen.model));
                    let count = async || {
                        let (status, n) = call(
                            format!("/json/2/{}/search_count", screen.model),
                            serde_json::json!({"domain": []}),
                            &admin,
                        )
                        .await;
                        assert_eq!(status, 200, "{} search_count: {n}", screen.model);
                        n.as_i64().unwrap_or_else(|| {
                            panic!("{} search_count is not an integer: {n}", screen.model)
                        })
                    };
                    let before = count().await;
                    let (create_vals, write_vals) = write_probe(screen.model, &probe_ids);
                    for (method, body, verb) in [
                        (
                            "create",
                            serde_json::json!({"vals_list": [create_vals]}),
                            "create",
                        ),
                        (
                            "write",
                            serde_json::json!({"ids": [record], "vals": write_vals}),
                            "modify",
                        ),
                        ("unlink", serde_json::json!({"ids": [record]}), "delete"),
                        // the browser's Save button (`crates/erp-orm/src/web/save.rs`)
                        (
                            "web_save",
                            serde_json::json!({
                                "ids": [record],
                                "vals": write_vals,
                                "specification": {},
                            }),
                            "modify",
                        ),
                        // Duplicate (`crates/erp-orm/src/rpc/core.rs`, `base.copy`)
                        ("copy", serde_json::json!({"ids": [record]}), "create"),
                    ] {
                        let (status, error) =
                            call(format!("/json/2/{}/{method}", screen.model), body, &admin).await;
                        // `ir.actions.actions` overrides create, write and unlink with its own
                        // refusal (`addons/base/src/ir_actions/mod.rs`, `key_table_refusal`);
                        // the override answers before the ACL check, for every caller
                        let (want_status, want_name, want_message) = if screen.model
                            == "ir.actions.actions"
                        {
                            (
                                500,
                                "erp.programming",
                                "ir.actions.actions rows follow their typed action; write \
                                     the typed action instead"
                                    .to_owned(),
                            )
                        } else {
                            (
                                403,
                                "erp.exceptions.AccessError",
                                model_denial(verb, &model_label(screen.model), screen.model, &[]),
                            )
                        };
                        assert_eq!(status, want_status, "{} {method}: {error}", screen.model);
                        assert_eq!(error["name"], want_name, "{} {method}", screen.model);
                        assert_eq!(error["message"], want_message, "{} {method}", screen.model);
                    }
                    assert_eq!(
                        count().await,
                        before,
                        "{}: nothing created or deleted",
                        screen.model
                    );
                }
            }
        }
    }

    secret_parameter_probes(&call, &admin, &ordinary, &secrets, &orm).await;

    // 3. An internal user without base.group_system: no Technical menu, and the actions are
    //    refused even when called directly.
    let (status, tree) = call(
        "/json/2/ir.ui.menu/erp_load_web_menus".to_owned(),
        serde_json::json!({}),
        &ordinary,
    )
    .await;
    assert_eq!(status, 200, "{tree}");
    let user_menus = menu_nodes(&tree);
    assert!(!user_menus.contains_key(&technical_menu));
    let mut action_probes = 0usize;
    for (screen, ids) in TECHNICAL_SCREENS.iter().zip(&loaded_screens) {
        assert!(!user_menus.contains_key(&ids.menu), "{}", screen.menu);
        let (status, body) = call(
            "/json/2/ir.actions.act_window/erp_load_web_action".to_owned(),
            serde_json::json!({"ids": [ids.action]}),
            &ordinary,
        )
        .await;
        assert_eq!(status, 403, "{} internal action: {body}", screen.action);
        assert_eq!(
            body["name"], "erp.exceptions.AccessError",
            "{} internal action",
            screen.action
        );
        action_probes += 1;
    }
    // every one of the 20 own screens was probed (a short `loaded_screens` would end the zip early)
    assert_eq!(action_probes, 20, "internal-user action probes");
    let mut read_probes = 0usize;
    for screen in &TECHNICAL_SCREENS {
        let groups: &[&str] = match screen.admin {
            // granted before r3, some also to internal users (menus, window actions): not probed here
            AdminRead::Granted => continue,
            AdminRead::ReadOnly | AdminRead::Managed => &["Settings Administration"],
        };
        let (status, body) = call(
            format!("/json/2/{}/web_search_read", screen.model),
            serde_json::json!({"domain": [], "specification": {}, "limit": 2}),
            &ordinary,
        )
        .await;
        assert_eq!(status, 403, "{} internal read: {body}", screen.model);
        assert_eq!(
            body["name"], "erp.exceptions.AccessError",
            "{} internal read",
            screen.model
        );
        assert_eq!(
            body["message"],
            model_denial("access", &model_label(screen.model), screen.model, groups),
            "{} internal read",
            screen.model
        );
        read_probes += 1;
    }
    // the ten read-only models of the lead's r3 decision plus `ir.config_parameter` (Managed)
    assert_eq!(read_probes, 11, "internal-user read probes");

    drop(service);
    drop(auth);
    drop(orm);
    drop(init);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_uom_015_seed_units_and_packaging_default_data_installed() {
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
        .code([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .unwrap();
    let _init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = system.system_env(Context::new());
    let installed = system
        .button_immediate_install(&env, &[ModuleName::new("uom").unwrap()])
        .await
        .unwrap();
    assert_eq!(installed.installed, vec![ModuleName::new("uom").unwrap()]);
    assert!(installed.post_commit.failed.is_empty());

    let client = db.client().await.unwrap();

    // Verify decimal.precision 'Product Unit' exists with digits 2
    let precision = client
        .query_one(
            "SELECT digits FROM decimal_precision WHERE name='Product Unit'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(precision.get::<_, i32>(0), 2);

    // Verify 30 units exist in uom_uom
    let uom_count: i64 = client
        .query_one("SELECT count(*) FROM uom_uom", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(uom_count, 30, "uom.uom catalog must have exactly 30 units");

    // Verify root units Units, Hours, mm, m², ml, g, KWH exist
    let roots = client
        .query(
            "SELECT name->>'en_US' FROM uom_uom WHERE relative_uom_id IS NULL ORDER BY name->>'en_US'",
            &[],
        )
        .await
        .unwrap();
    let root_names: Vec<String> = roots.iter().map(|r| r.get(0)).collect();
    assert!(root_names.contains(&"Units".to_owned()));
    assert!(root_names.contains(&"Hours".to_owned()));
    assert!(root_names.contains(&"mm".to_owned()));
    assert!(root_names.contains(&"m²".to_owned()));
    assert!(root_names.contains(&"ml".to_owned()));
    assert!(root_names.contains(&"g".to_owned()));
    assert!(root_names.contains(&"KWH".to_owned()));

    // PD-R1: count/root checks cannot prove required children or their relative parents.
    // The metric tonne uses our catalog's SI label "t" (SPEC-UOM §5 seed matrix).
    let children = client.query(
        "SELECT d.name, u.name->>'en_US', p.name FROM ir_model_data d \
         JOIN uom_uom u ON u.id=d.res_id \
         LEFT JOIN ir_model_data p ON p.model='uom.uom' AND p.module='uom' AND p.res_id=u.relative_uom_id \
         WHERE d.module='uom' AND d.model='uom.uom' \
         AND d.name IN ('product_uom_day','product_uom_dozen','product_uom_kgm','product_uom_ton') ORDER BY d.name",
        &[],
    ).await.unwrap();
    let children: Vec<(String, String, String)> = children
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    assert_eq!(
        children,
        vec![
            (
                "product_uom_day".to_owned(),
                "Days".to_owned(),
                "product_uom_hour".to_owned()
            ),
            (
                "product_uom_dozen".to_owned(),
                "Dozens".to_owned(),
                "product_uom_unit".to_owned()
            ),
            (
                "product_uom_kgm".to_owned(),
                "kg".to_owned(),
                "product_uom_gram".to_owned()
            ),
            (
                "product_uom_ton".to_owned(),
                "t".to_owned(),
                "product_uom_kgm".to_owned()
            ),
        ]
    );

    // Verify CLI runtime linked addon identity includes uom and product
    let native_ids = erp_cli::identity::native_artifact_identities().unwrap();
    assert!(
        native_ids.contains_key(&ModuleName::new("uom").unwrap()),
        "uom must be included in CLI linked native artifact identities"
    );
    assert!(
        native_ids.contains_key(&ModuleName::new("product").unwrap()),
        "product must be included in CLI linked native artifact identities"
    );

    let acls = client
        .query(
            "SELECT g.name, a.operation \
             FROM ir_access a \
             JOIN ir_model m ON m.id = a.model_id \
             JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id AND g.module = 'base' \
             WHERE m.model = 'uom.uom' \
             ORDER BY g.name",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        acls.len(),
        2,
        "uom.uom must have manager and user access rules"
    );
    assert_eq!(acls[0].get::<_, String>(0), "group_system");
    assert_eq!(acls[0].get::<_, String>(1), "crud");
    assert_eq!(acls[1].get::<_, String>(0), "group_user");
    assert_eq!(acls[1].get::<_, String>(1), "r");

    drop(client);
    drop(env);
    drop(system);
    db.cleanup().await.unwrap();
}

fn company_partner_migration_system(db: &TestDb) -> ModuleSystem {
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
    ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(native_artifact_identities)
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([&erp_addon_base::ADDON])
        .build()
        .unwrap()
}

async fn company_partner_migration_fixture() -> (TestDb, ModuleSystem) {
    let db = TestDb::create().await.unwrap();
    let system = company_partner_migration_system(&db);
    let initialized = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert!(initialized.post_commit.is_clean());
    drop(initialized);
    (db, system)
}

#[tokio::test]
async fn test_base_1_1_migration_populates_missing_company_partner_id() {
    // Keep the shared initialization future off the default test-thread stack.
    let (db, system) = Box::pin(company_partner_migration_fixture()).await;
    let client = db.client().await.unwrap();

    // Simulate pre-existing database with multiple companies:
    // 1. main_company with partner_id = NULL
    // 2. branch_company with partner_id = NULL (must receive a dedicated partner, not main_partner)
    // 3. existing_company with an existing non-null partner_id (must remain untouched)
    // 4. base module latest_version = '1.0.0' to simulate upgrading from base 1.0 to 1.1
    client
        .execute(
            "UPDATE ir_module_module SET latest_version = '1.0.0' WHERE name = 'base'",
            &[],
        )
        .await
        .unwrap();
    let main_comp_id: i64 = client
        .query_one(
            "SELECT c.id FROM res_company c JOIN ir_model_data d ON d.res_id = c.id \
             WHERE d.module = 'base' AND d.name = 'main_company' AND d.model = 'res.company'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let currency_id: i64 = client
        .query_one(
            "SELECT currency_id FROM res_company WHERE id = $1",
            &[&main_comp_id],
        )
        .await
        .unwrap()
        .get(0);

    client
        .execute(
            "UPDATE res_company SET partner_id = NULL WHERE id = $1",
            &[&main_comp_id],
        )
        .await
        .unwrap();

    let branch_comp_id: i64 = client
        .query_one(
            "INSERT INTO res_company (name, currency_id, partner_id) VALUES ('Branch Company', $1, NULL) RETURNING id",
            &[&currency_id],
        )
        .await
        .unwrap()
        .get(0);

    // Seed an existing unrelated contact belonging to branch_company (company_id is ownership, not representation)
    let unrelated_contact_id: i64 = client
        .query_one(
            "INSERT INTO res_partner (name, company_id, active, partner_share) VALUES ('Unrelated Branch Contact', $1, true, false) RETURNING id",
            &[&branch_comp_id],
        )
        .await
        .unwrap()
        .get(0);

    let custom_partner_id: i64 = client
        .query_one(
            "INSERT INTO res_partner (name, active, partner_share) VALUES ('Custom Partner', true, false) RETURNING id",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    let existing_comp_id: i64 = client
        .query_one(
            "INSERT INTO res_company (name, currency_id, partner_id) VALUES ('Existing Company', $1, $2) RETURNING id",
            &[&currency_id, &custom_partner_id],
        )
        .await
        .unwrap()
        .get(0);

    client
        .execute(
            "UPDATE ir_model_data SET noupdate = false WHERE module = 'base' AND model = 'res.currency'",
            &[],
        )
        .await
        .unwrap();

    // Raw fixture inserts bypass the old ORM's parent-store maintenance. Reproduce valid
    // legacy root paths before testing the independent partner-link migration.
    client.execute("UPDATE res_company SET parent_path=id::text || '/' WHERE parent_id IS NULL AND parent_path IS NULL", &[]).await.unwrap();
    // Perform module upgrade to trigger base 1.1 forward migration
    let env = system.system_env(Context::new());
    let upgraded = system
        .button_immediate_upgrade(&env, &[ModuleName::new("base").unwrap()])
        .await
        .unwrap();
    assert!(upgraded.post_commit.failed.is_empty());

    // 1. Verify main_company partner_id links to base.main_partner
    let main_partner_id: i64 = client
        .query_one(
            "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = 'main_partner' AND model = 'res.partner'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let main_comp_partner: Option<i64> = client
        .query_one(
            "SELECT partner_id FROM res_company WHERE id = $1",
            &[&main_comp_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        main_comp_partner,
        Some(main_partner_id),
        "main_company must be linked to base.main_partner"
    );

    // 2. Verify branch_company received a distinct dedicated partner (NOT main_partner and NOT unrelated contact)
    let branch_comp_partner: Option<i64> = client
        .query_one(
            "SELECT partner_id FROM res_company WHERE id = $1",
            &[&branch_comp_id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        branch_comp_partner.is_some()
            && branch_comp_partner != Some(main_partner_id)
            && branch_comp_partner != Some(unrelated_contact_id),
        "branch_company must receive a dedicated partner distinct from main_partner and unrelated contact"
    );
    let branch_partner_row = client
        .query_one(
            "SELECT name, company_id FROM res_partner WHERE id = $1",
            &[&branch_comp_partner.unwrap()],
        )
        .await
        .unwrap();
    assert_eq!(branch_partner_row.get::<_, String>(0), "Branch Company");
    assert_eq!(branch_partner_row.get::<_, i64>(1), branch_comp_id);

    // Combined CORE contract: a dedicated root must be created with ordinary defaults
    // and stored computes, not just a name/company row inserted behind the ORM.
    let branch_partner_id = branch_comp_partner.unwrap();
    let computed = client.query_one(
        "SELECT commercial_partner_id, complete_name, commercial_company_name, type, active, lang FROM res_partner WHERE id=$1",
        &[&branch_partner_id],
    ).await.unwrap();
    assert_eq!(
        computed.get::<_, Option<i64>>(0),
        Some(branch_partner_id),
        "migration-created root must be its own commercial partner"
    );
    assert_eq!(
        computed.get::<_, Option<String>>(1).as_deref(),
        Some("Branch Company")
    );
    assert_eq!(
        computed.get::<_, Option<String>>(2).as_deref(),
        Some("Branch Company")
    );
    assert_eq!(
        computed.get::<_, Option<String>>(3).as_deref(),
        Some("contact")
    );
    assert!(computed.get::<_, bool>(4));
    assert_eq!(
        computed.get::<_, Option<String>>(5).as_deref(),
        Some("en_US")
    );
    let mut edit = upgraded.orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let partner = edit
        .env()
        .records("res.partner")
        .unwrap()
        .browse([branch_partner_id])
        .unwrap();
    partner
        .write(
            &mut edit,
            erp_sdk::RawVals::new()
                .set_cell("name", erp_sdk::Cell::Str("Renamed representative".into())),
        )
        .await
        .unwrap();
    edit.commit().await.unwrap();
    let renamed = client
        .query_one(
            "SELECT complete_name, commercial_company_name FROM res_partner WHERE id=$1",
            &[&branch_partner_id],
        )
        .await
        .unwrap();
    assert_eq!(
        renamed.get::<_, Option<String>>(0).as_deref(),
        Some("Renamed representative")
    );
    assert_eq!(
        renamed.get::<_, Option<String>>(1).as_deref(),
        Some("Renamed representative")
    );

    // Verify existing unrelated contact is untouched
    let unrelated_contact_row = client
        .query_one(
            "SELECT name, company_id FROM res_partner WHERE id = $1",
            &[&unrelated_contact_id],
        )
        .await
        .unwrap();
    assert_eq!(
        unrelated_contact_row.get::<_, String>(0),
        "Unrelated Branch Contact"
    );
    assert_eq!(unrelated_contact_row.get::<_, i64>(1), branch_comp_id);

    // 3. Verify existing_company preserved its pre-existing custom_partner_id
    let existing_comp_partner: Option<i64> = client
        .query_one(
            "SELECT partner_id FROM res_company WHERE id = $1",
            &[&existing_comp_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        existing_comp_partner,
        Some(custom_partner_id),
        "existing non-null company partner link must be preserved"
    );

    // Verify ir_model_data noupdate flag has been set to true for currencies
    let currency_noupdate: bool = client
        .query_one(
            "SELECT COALESCE(noupdate, false) FROM ir_model_data WHERE module = 'base' AND model = 'res.currency' LIMIT 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        currency_noupdate,
        "forward migration must set noupdate = true for currency external ids"
    );

    // 4. Verify idempotency on second upgrade
    let partner_count_before: i64 = client
        .query_one("SELECT count(*) FROM res_partner", &[])
        .await
        .unwrap()
        .get(0);

    let rerun = system
        .button_immediate_upgrade(&env, &[ModuleName::new("base").unwrap()])
        .await
        .unwrap();
    assert!(rerun.post_commit.failed.is_empty());

    let main_comp_partner_after: Option<i64> = client
        .query_one(
            "SELECT partner_id FROM res_company WHERE id = $1",
            &[&main_comp_id],
        )
        .await
        .unwrap()
        .get(0);
    let branch_comp_partner_after: Option<i64> = client
        .query_one(
            "SELECT partner_id FROM res_company WHERE id = $1",
            &[&branch_comp_id],
        )
        .await
        .unwrap()
        .get(0);
    let existing_comp_partner_after: Option<i64> = client
        .query_one(
            "SELECT partner_id FROM res_company WHERE id = $1",
            &[&existing_comp_id],
        )
        .await
        .unwrap()
        .get(0);
    let partner_count_after: i64 = client
        .query_one("SELECT count(*) FROM res_partner", &[])
        .await
        .unwrap()
        .get(0);

    assert_eq!(main_comp_partner, main_comp_partner_after);
    assert_eq!(branch_comp_partner, branch_comp_partner_after);
    assert_eq!(existing_comp_partner, existing_comp_partner_after);
    assert_eq!(
        partner_count_before, partner_count_after,
        "rerun upgrade must be idempotent without creating duplicate partners"
    );

    drop(client);
    drop(env);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn combined_migration_rolls_back_partial_partner_creation() {
    // Keep the shared initialization future off the default test-thread stack.
    let (db, system) = Box::pin(company_partner_migration_fixture()).await;
    let client = db.client().await.unwrap();
    client
        .execute(
            "UPDATE ir_module_module SET latest_version='1.0.0' WHERE name='base'",
            &[],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE ir_model_data SET noupdate=false WHERE module='base' AND model='res.currency'",
            &[],
        )
        .await
        .unwrap();
    let currency: i64 = client
        .query_one(
            "SELECT currency_id FROM res_company ORDER BY id LIMIT 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let mut companies = Vec::new();
    for name in ["Rollback first company", "Rollback second company"] {
        companies.push(
            client
                .query_one(
                    "INSERT INTO res_company(name,currency_id) VALUES($1,$2) RETURNING id",
                    &[&name, &currency],
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
        );
    }
    // Raw fixture inserts bypass the old ORM's parent-store maintenance. Reproduce valid
    // legacy root paths before testing the independent partner-link migration.
    client.execute("UPDATE res_company SET parent_path=id::text || '/' WHERE parent_id IS NULL AND parent_path IS NULL", &[]).await.unwrap();
    let before: i64 = client
        .query_one("SELECT count(*) FROM res_partner", &[])
        .await
        .unwrap()
        .get(0);
    client
        .batch_execute(
            r#"
        CREATE FUNCTION fixture_reject_second_partner() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF NEW.name = 'Rollback second company' THEN
            IF EXISTS (SELECT 1 FROM res_partner WHERE name='Rollback first company') THEN
              RAISE EXCEPTION 'fixture: rejecting after earlier root insert';
            END IF;
            RAISE EXCEPTION 'fixture: rejected before earlier root insert';
          END IF;
          RETURN NEW;
        END $$;
        CREATE TRIGGER fixture_reject_second_partner BEFORE INSERT ON res_partner
        FOR EACH ROW EXECUTE FUNCTION fixture_reject_second_partner();
    "#,
        )
        .await
        .unwrap();
    let error = system
        .button_immediate_upgrade(
            &system.system_env(Context::new()),
            &[ModuleName::new("base").unwrap()],
        )
        .await
        .unwrap_err();
    let detail = format!("{error:#?}");
    assert!(
        detail.contains("rejecting after earlier root insert"),
        "{detail}"
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM res_partner", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        before
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM res_company WHERE id=ANY($1) AND partner_id IS NULL",
                &[&companies]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='base'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "1.0.0"
    );
    assert!(!client.query_one("SELECT bool_or(noupdate) FROM ir_model_data WHERE module='base' AND model='res.currency'", &[]).await.unwrap().get::<_,bool>(0));
    client.batch_execute("DROP TRIGGER fixture_reject_second_partner ON res_partner; DROP FUNCTION fixture_reject_second_partner()").await.unwrap();
    let recovered = system
        .button_immediate_upgrade(
            &system.system_env(Context::new()),
            &[ModuleName::new("base").unwrap()],
        )
        .await
        .unwrap();
    assert!(recovered.post_commit.is_clean());
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM res_partner", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        before + 2
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(DISTINCT partner_id) FROM res_company WHERE id=ANY($1)",
                &[&companies]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    drop(recovered);
    drop(client);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn combined_migration_concurrent_upgrade_is_refused_without_duplicate_partners() {
    // Keep the shared initialization future off the default test-thread stack.
    let (db, system) = Box::pin(company_partner_migration_fixture()).await;
    let other_system = company_partner_migration_system(&db);
    let client = db.client().await.unwrap();
    client
        .execute(
            "UPDATE ir_module_module SET latest_version='1.0.0' WHERE name='base'",
            &[],
        )
        .await
        .unwrap();
    let currency: i64 = client
        .query_one(
            "SELECT currency_id FROM res_company ORDER BY id LIMIT 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let company: i64 = client.query_one("INSERT INTO res_company(name,currency_id) VALUES('Concurrent migration company',$1) RETURNING id", &[&currency]).await.unwrap().get(0);
    // Raw fixture inserts bypass the old ORM's parent-store maintenance. Reproduce valid
    // legacy root paths before testing the independent partner-link migration.
    client.execute("UPDATE res_company SET parent_path=id::text || '/' WHERE parent_id IS NULL AND parent_path IS NULL", &[]).await.unwrap();
    let before: i64 = client
        .query_one("SELECT count(*) FROM res_partner", &[])
        .await
        .unwrap()
        .get(0);
    client
        .batch_execute(
            r#"
        CREATE FUNCTION fixture_pause_migrated_partner() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF NEW.name='Concurrent migration company' THEN
            PERFORM pg_advisory_xact_lock(1124991,97);
          END IF;
          RETURN NEW;
        END $$;
        CREATE TRIGGER fixture_pause_migrated_partner BEFORE INSERT ON res_partner
        FOR EACH ROW EXECUTE FUNCTION fixture_pause_migrated_partner();
    "#,
        )
        .await
        .unwrap();
    let mut holder = db.client().await.unwrap();
    let hold = holder.transaction().await.unwrap();
    hold.query_one("SELECT pg_advisory_xact_lock(1124991,97)", &[])
        .await
        .unwrap();
    let first = tokio::spawn(async move {
        system
            .button_immediate_upgrade(
                &system.system_env(Context::new()),
                &[ModuleName::new("base").unwrap()],
            )
            .await
    });
    // The loop ends as soon as the upgrade waits on the barrier; the deadline only bounds a run that never
    // gets there. 10 s was too short on a loaded host (failed there on unchanged code, 2026-11-13).
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    let paused = loop {
        let waiting: bool = client.query_one("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND NOT granted AND classid=1124991::oid AND objid=97::oid AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))", &[]).await.unwrap().get(0);
        if waiting {
            break true;
        }
        if tokio::time::Instant::now() >= until {
            break false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    let second = if paused {
        Some(
            other_system
                .button_immediate_upgrade(
                    &other_system.system_env(Context::new()),
                    &[ModuleName::new("base").unwrap()],
                )
                .await,
        )
    } else {
        None
    };
    hold.rollback().await.unwrap();
    let first_result = first.await.unwrap();
    assert!(
        paused,
        "first runtime did not reach the migration insertion barrier: {first_result:?}"
    );
    let second_error = second.unwrap().unwrap_err();
    assert!(matches!(
        second_error,
        erp_modules::Error::OperationInProgress
    ));
    let first_result = first_result.unwrap();
    assert!(first_result.post_commit.is_clean());
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM res_partner", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        before + 1
    );
    let representative: i64 = client
        .query_one(
            "SELECT partner_id FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()
        .get(0);
    let repeated = other_system
        .button_immediate_upgrade(
            &other_system.system_env(Context::new()),
            &[ModuleName::new("base").unwrap()],
        )
        .await
        .unwrap();
    assert!(repeated.post_commit.is_clean());
    assert_eq!(
        client
            .query_one(
                "SELECT partner_id FROM res_company WHERE id=$1",
                &[&company]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        representative
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM res_partner", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        before + 1
    );
    drop(repeated);
    drop(first_result);
    drop(holder);
    drop(client);
    drop(other_system);
    db.cleanup().await.unwrap();
}
