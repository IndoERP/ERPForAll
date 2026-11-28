// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installing `hr` into a fresh database: the tables, the views, the actions, the menus and the
//! access rows exist, a job or an employee created without `active` is active, an employee needs
//! a company, and a user is linked to at most one employee per company.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::path::Path;

use erp_fields::Cell;
use erp_fields::ErrorKind;
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Context, RawVals, SUPERUSER_ID};
use erp_testkit::TestDb;

const STACK_SIZE: usize = 16 * 1024 * 1024;

fn system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../addons")
            .canonicalize()
            .unwrap(),
    );
    let identities = index
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
    ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_hr::ADDON,
        ])
        .build()
        .unwrap()
}

fn hr_version() -> &'static str {
    include_str!("../../../addons/hr/manifest.toml")
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap()
}

async fn names(client: &tokio_postgres::Client, model: &str) -> Vec<String> {
    client
        .query(
            "SELECT name FROM ir_model_data WHERE module = 'hr' AND model = $1::text ORDER BY name",
            &[&model],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// The declared (not generated) access rows of `model` as `<group>/<operations>`.
async fn access(client: &tokio_postgres::Client, model: &str) -> Vec<String> {
    client
        .query(
            "SELECT coalesce(g.module || '.' || g.name, '-') || '/' || a.operation \
             FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
             LEFT JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
             WHERE m.model = $1::text AND NOT coalesce(a.generated, false) ORDER BY 1",
            &[&model],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn base_xid(client: &tokio_postgres::Client, name: &str) -> i64 {
    client
        .query_one(
            "SELECT res_id::int8 FROM ir_model_data WHERE module = 'base' AND name = $1",
            &[&name],
        )
        .await
        .unwrap()
        .get(0)
}

fn employee(name: &str, company: i64, user: Option<i64>) -> RawVals {
    let vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("company_id", Cell::Id(company));
    match user {
        Some(id) => vals.set_cell("user_id", Cell::Id(id)),
        None => vals,
    }
}

/// Employees: active by default, a company is required, and one user is linked to at most one
/// employee per company (any number of employees have no user).
async fn employees(orm: &erp_orm::Orm, client: &tokio_postgres::Client) -> i64 {
    let company = base_xid(client, "main_company").await;
    let admin_group = base_xid(client, "group_system").await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let currency = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([company])
        .unwrap()
        .cell(&mut tx, "currency_id")
        .await
        .unwrap();
    let other_company = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Second Company".into()))
                    .set_cell("currency_id", currency),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let user = tx
        .env()
        .records("res.users")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Employee Admin".into()))
                    .set_cell("login", Cell::Str("employee-admin".into()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_commands(
                        "company_ids",
                        [Command::Set(vec![
                            erp_fields::RecId::from_real(company).unwrap(),
                            erp_fields::RecId::from_real(other_company).unwrap(),
                        ])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::Set(vec![
                            erp_fields::RecId::from_real(admin_group).unwrap(),
                        ])],
                    ),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let employees = tx.env().records("hr.employee").unwrap();
    let created = employees
        .create(
            &mut tx,
            vec![
                employee("Ada", company, Some(user)),
                employee("Ben", company, None),
                employee("Cy", company, None),
                employee("Ada (second company)", other_company, Some(user)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(created.ids().len(), 4);
    tx.commit().await.unwrap();
    // a finished transaction keeps its connection until it is dropped
    drop(tx);
    let active: Vec<bool> = client
        .query("SELECT active FROM hr_employee ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(active, [true; 4], "a new employee is active by default");

    // A second employee of the same company for the same user is refused.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx
        .env()
        .records("hr.employee")
        .unwrap()
        .create(&mut tx, vec![employee("Ada again", company, Some(user))])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    assert!(
        err.message()
            .contains("This user is already linked to another employee of this company."),
        "{err}"
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // An employee without a company is refused.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx
        .env()
        .records("hr.employee")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Nobody's".into()))],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    tx.rollback().await.unwrap();
    drop(tx);
    let count: i64 = client
        .query_one("SELECT count(*) FROM hr_employee", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 4, "the refused employees were not stored");
    user
}

async fn install() {
    let db = TestDb::create().await.unwrap();
    let system = system(&db);
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let outcome =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("hr").unwrap()]))
            .await
            .unwrap();
    assert!(outcome.post_commit.is_clean());

    let client = db.client().await.unwrap();
    assert_eq!(
        names(&client, "ir.ui.view").await,
        [
            "employee_overview_list",
            "hr_department_view_form",
            "hr_department_view_hierarchy",
            "hr_department_view_kanban",
            "hr_department_view_list",
            "hr_department_view_search",
            "hr_departure_reason_view_form",
            "hr_departure_reason_view_list",
            "hr_departure_wizard_view_form",
            "hr_employee_category_view_form",
            "hr_employee_category_view_list",
            "hr_employee_skill_view_form",
            "hr_employee_skill_view_graph",
            "hr_employee_skill_view_list",
            "hr_employee_skill_view_pivot",
            "hr_employee_skill_view_search",
            "hr_employee_type_view_form",
            "hr_employee_type_view_list",
            "hr_employee_version_view_form",
            "hr_employee_version_view_list",
            "hr_employee_view_activity",
            "hr_employee_view_form",
            "hr_employee_view_graph",
            "hr_employee_view_hierarchy",
            "hr_employee_view_kanban",
            "hr_employee_view_list",
            "hr_employee_view_pivot",
            "hr_employee_view_search",
            "hr_job_view_form",
            "hr_job_view_kanban",
            "hr_job_view_list",
            "hr_job_view_search",
            "hr_resume_line_type_view_form",
            "hr_resume_line_type_view_list",
            "hr_resume_line_view_form",
            "hr_resume_line_view_list",
            "hr_skill_level_view_form",
            "hr_skill_level_view_list",
            "hr_skill_type_view_form",
            "hr_skill_type_view_list",
            "hr_skill_view_form",
            "hr_skill_view_list",
            "hr_work_location_view_form",
            "hr_work_location_view_list",
            "hr_work_schedule_view_form",
            "hr_work_schedule_view_list",
            "report_employee_badge",
            "report_employee_badge_document",
        ]
    );
    assert_eq!(
        names(&client, "ir.actions.act_window").await,
        [
            "employee_overview_action",
            "hr_department_action",
            "hr_departure_reason_action",
            "hr_employee_action",
            "hr_employee_category_action",
            "hr_employee_directory_action",
            "hr_employee_reporting_action",
            "hr_employee_skill_action",
            "hr_employee_type_action",
            "hr_employee_version_action",
            "hr_job_action",
            "hr_resume_line_action",
            "hr_resume_line_type_action",
            "hr_skill_type_action",
            "hr_skills_reporting_action",
            "hr_work_location_action",
            "hr_work_schedule_action",
        ]
    );
    assert_eq!(
        names(&client, "ir.ui.menu").await,
        [
            "hr_config_employee_menu",
            "hr_config_recruitment_menu",
            "hr_config_skills_menu",
            "hr_config_time_menu",
            "hr_configuration_menu",
            "hr_department_menu",
            "hr_departure_reason_menu",
            "hr_directory_menu",
            "hr_employee_category_menu",
            "hr_employee_menu",
            "hr_employee_skill_menu",
            "hr_employee_type_menu",
            "hr_job_menu",
            "hr_learning_menu",
            "hr_menu_root",
            "hr_overview_menu",
            "hr_reporting_contracts_menu",
            "hr_reporting_headcount_menu",
            "hr_reporting_menu",
            "hr_reporting_skills_menu",
            "hr_resume_line_menu",
            "hr_resume_line_type_menu",
            "hr_skill_type_menu",
            "hr_work_location_menu",
            "hr_work_schedule_menu",
        ]
    );

    // Default data: every configuration model that has defaults ships them; tags ship none.
    assert_eq!(
        names(&client, "hr.department").await,
        ["hr_department_administration"]
    );
    assert_eq!(
        names(&client, "hr.work.location").await,
        [
            "hr_work_location_home",
            "hr_work_location_office",
            "hr_work_location_other",
        ]
    );
    assert!(names(&client, "hr.employee.category").await.is_empty());
    for (model, table) in [
        ("hr.department", "hr_department"),
        ("hr.work.location", "hr_work_location"),
        ("hr.employee.type", "hr_employee_type"),
        ("hr.work.schedule", "hr_work_schedule"),
        ("hr.departure.reason", "hr_departure_reason"),
        ("hr.resume.line.type", "hr_resume_line_type"),
    ] {
        let seeded = names(&client, model).await.len();
        assert!(seeded > 0, "{model} ships default records");
        let stored: i64 = client
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(usize::try_from(stored).unwrap(), seeded, "{model}");
    }

    // The installed version is the manifest's.
    let installed: String = client
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name = 'hr'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(installed, hr_version());

    for model in ["hr.job", "hr.employee", "hr.department"] {
        assert_eq!(
            access(&client, model).await,
            [
                "base.group_user/r",
                "hr.group_hr_manager/crud",
                "hr.group_hr_user/cru",
            ],
            "{model}"
        );
    }

    let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
    let created = tx
        .env()
        .records("hr.job")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Engineer".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    let job: bool = client
        .query_one("SELECT active FROM hr_job WHERE id = $1", &[&created])
        .await
        .unwrap()
        .get(0);
    assert!(job, "a new job position is active by default");
    drop(tx);

    let admin_uid = employees(&outcome.orm, &client).await;

    // Verify every HR model's declared views lower cleanly through H18 (`get_views`) with a real policy snapshot.
    let loaded = Box::pin(system.load()).await.unwrap();
    let registry = std::sync::Arc::clone(loaded.orm.registry());
    let policy_source = erp_security::source::PolicySource::build(
        std::sync::Arc::clone(&registry),
        db.pool().clone(),
    )
    .await
    .unwrap();
    let policy_orm = erp_orm::Orm::builder(std::sync::Arc::clone(&registry), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            std::sync::Arc::clone(&registry),
            loaded
                .orm
                .env(SUPERUSER_ID, Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .resolver(erp_security::resolver::Resolver::for_registry(&registry).unwrap())
        .policy(std::sync::Arc::new(policy_source) as std::sync::Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .build()
        .unwrap();
    let mut tx = policy_orm
        .begin(erp_sdk::UserId::new(admin_uid).unwrap())
        .await
        .unwrap();
    for (model, kinds) in [
        (
            "hr.employee",
            &[
                "kanban",
                "list",
                "hierarchy",
                "activity",
                "graph",
                "pivot",
                "form",
                "search",
            ][..],
        ),
        (
            "hr.department",
            &["kanban", "list", "hierarchy", "form", "search"][..],
        ),
        ("hr.job", &["list", "kanban", "form", "search"][..]),
        ("hr.employee.type", &["list", "form"][..]),
        ("hr.work.location", &["list", "form"][..]),
        ("hr.work.schedule", &["list", "form"][..]),
        ("hr.departure.reason", &["list", "form"][..]),
        ("hr.employee.category", &["list", "form"][..]),
        ("hr.employee.version", &["list", "form"][..]),
        ("hr.resume.line.type", &["list", "form"][..]),
        ("hr.resume.line", &["list", "form"][..]),
        ("hr.skill.type", &["list", "form"][..]),
        ("hr.skill", &["list", "form"][..]),
        ("hr.skill.level", &["list", "form"][..]),
        (
            "hr.employee.skill",
            &["list", "pivot", "graph", "form", "search"][..],
        ),
        ("hr.departure.wizard", &["form"][..]),
    ] {
        let requests = kinds
            .iter()
            .map(|kind| {
                erp_fields::Value::list([erp_fields::Value::False, erp_fields::Value::str(*kind)])
            })
            .collect();
        let lowered = erp_addon_web::metadata_test_support::test_get_views(
            &tx.env(),
            &mut tx,
            model,
            requests,
            32 * 1024 * 1024,
        )
        .await;
        assert!(
            lowered.is_ok(),
            "failed to lower {model} views {kinds:?}: {:?}",
            lowered.err()
        );
    }
    tx.rollback().await.unwrap();
}

#[test]
fn hr_installs_its_model_views_menus_and_access() {
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(install());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn hr_demo_loads_cleanly() {
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let db = TestDb::create().await.unwrap();
                    let system = system(&db);
                    let init = Box::pin(
                        system.initialize(
                            InitOptions::new()
                                .with_skip_auto_install(true)
                                .with_demo(true),
                        ),
                    )
                    .await
                    .unwrap();
                    let env = init.orm.env(SUPERUSER_ID, Context::new());
                    let outcome = Box::pin(
                        system.button_immediate_install(&env, &[ModuleName::new("hr").unwrap()]),
                    )
                    .await
                    .unwrap();
                    assert!(
                        outcome.data_notices.is_empty(),
                        "unexpected demo notices: {:?}",
                        outcome.data_notices
                    );
                    let client = db.client().await.unwrap();
                    let emp_count: i64 = client
                        .query_one("SELECT count(*) FROM hr_employee", &[])
                        .await
                        .unwrap()
                        .get(0);
                    let dept_count: i64 = client
                        .query_one("SELECT count(*) FROM hr_department", &[])
                        .await
                        .unwrap()
                        .get(0);
                    assert_eq!(emp_count, 12, "expected 12 demo employees");
                    assert_eq!(
                        dept_count, 8,
                        "expected 7 demo departments and the default one"
                    );
                    let managed: i64 = client
                        .query_one(
                            "SELECT count(*) FROM hr_department d JOIN ir_model_data x \
                             ON x.model = 'hr.department' AND x.res_id = d.id \
                             WHERE x.name LIKE 'hr_demo_dept_%' AND d.manager_id IS NOT NULL",
                            &[],
                        )
                        .await
                        .unwrap()
                        .get(0);
                    assert_eq!(managed, 7, "every demo department has its manager");
                });
        })
        .unwrap()
        .join()
        .unwrap();
}
