// SPDX-License-Identifier: MIT OR Apache-2.0
//! Shared setup of the project suite: the module system with the `project` closure, a cached
//! installed template, an ORM that asks the installed access policy (so a test acts as a real
//! user, not the superuser), and the users and companies the tests need.

use std::path::Path;
use std::sync::Arc;

use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, Records, SUPERUSER_ID, Transaction};
use erp_testkit::{TemplateDb, TestDb};

/// A full install chain needs more than the default thread stack.
const STACK_SIZE: usize = 16 * 1024 * 1024;

/// The cached template: `project` installed without demo data. Change the key when the
/// template's content changes shape.
const TEMPLATE_KEY: &str = "erp-suite-project:project:v2";

/// The four template stages of the installed data, in board order:
/// (external id, name, sequence, folded).
pub const TEMPLATE_STAGES: [(&str, &str, i32, bool); 4] = [
    ("task_type_new", "New", 10, false),
    ("task_type_in_progress", "In Progress", 20, false),
    ("task_type_done", "Done", 30, true),
    ("task_type_cancelled", "Cancelled", 40, true),
];

/// The task `state` keys (D3).
pub const STATES: [&str; 5] = [
    "in_progress",
    "changes_requested",
    "approved",
    "done",
    "cancelled",
];

pub fn system_for_pool(pool: erp_testkit::deadpool_postgres::Pool) -> ModuleSystem {
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
    ModuleSystem::builder(pool, index)
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_mail::ADDON,
            // only its code: every module system needs the hooks `uom` declares, installed or not
            &erp_addon_uom::ADDON,
            &erp_addon_project::ADDON,
        ])
        .build()
        .unwrap()
}

/// Run `test` on a thread with a large stack and its own runtime.
pub fn run<F: Future<Output = ()> + 'static>(test: impl FnOnce() -> F + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(test());
        })
        .unwrap()
        .join()
        .unwrap();
}

pub fn project_module() -> ModuleName {
    ModuleName::new("project").unwrap()
}

/// A clone of the installed `project` template (no demo data) and the loaded superuser ORM of
/// its module system.
pub async fn cloned_installed() -> (TestDb, Orm) {
    let db = TemplateDb::clone_or_init(TEMPLATE_KEY, |pool| async move {
        let system = system_for_pool(pool);
        let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
            .await
            .map_err(|e| e.to_string())?;
        let env = init.orm.env(SUPERUSER_ID, Context::new());
        let outcome = Box::pin(system.button_immediate_install(&env, &[project_module()]))
            .await
            .map_err(|e| e.to_string())?;
        if !outcome.post_commit.is_clean() {
            return Err(format!("post_commit not clean: {:?}", outcome.post_commit));
        }
        if !outcome.data_notices.is_empty() {
            return Err(format!("install notices: {:?}", outcome.data_notices));
        }
        Ok::<(), String>(())
    })
    .await
    .unwrap();
    let system = system_for_pool(db.pool().clone());
    let loaded = Box::pin(system.load()).await.unwrap();
    let orm = loaded.orm.clone();
    (db, orm)
}

/// An ORM on the same database that asks the installed access policy and writes the chatter
/// history at commit, as a serving process does. Build it after the commit that adds the users:
/// the policy snapshot is read when it is built.
pub async fn policy_orm(db: &TestDb, installed: &Orm) -> Orm {
    let source = erp_security::source::PolicySource::build(
        Arc::clone(installed.registry()),
        db.pool().clone(),
    )
    .await
    .unwrap();
    Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(installed.registry()),
            installed
                .env(SUPERUSER_ID, Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .resolver(erp_security::resolver::Resolver::for_registry(installed.registry()).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        // the chatter history (creation lines, tracked changes), as the serving process has it
        .precommit_hook(erp_addon_mail::TRACKING_HOOK)
        .build()
        .unwrap()
}

/// The record id of the external id `module.name`.
pub async fn xid(client: &tokio_postgres::Client, module: &str, name: &str) -> i64 {
    client
        .query_one(
            "SELECT res_id::int8 FROM ir_model_data WHERE module = $1::text AND name = $2::text",
            &[&module, &name],
        )
        .await
        .unwrap_or_else(|e| panic!("external id {module}.{name}: {e}"))
        .get(0)
}

pub fn real(id: i64) -> RecId {
    RecId::from_real(id).unwrap()
}

pub fn named(name: &str) -> RawVals {
    RawVals::new().set_cell("name", Cell::Str(name.into()))
}

pub fn records(tx: &Transaction, model: &str, id: i64) -> Records {
    tx.env().records(model).unwrap().browse([id]).unwrap()
}

pub async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> i64 {
    tx.env()
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap_or_else(|e| panic!("creating {model}: {e}"))
        .ids()[0]
}

pub async fn cell(tx: &mut Transaction, model: &str, id: i64, field: &str) -> Cell {
    records(tx, model, id)
        .cell(tx, field)
        .await
        .unwrap_or_else(|e| panic!("reading {model}.{field} of {id}: {e}"))
}

/// The ids of an x2many cell, in their order.
pub fn ids(cell: &Cell) -> Vec<i64> {
    match cell {
        Cell::Ids(ids) => ids.iter().map(|id| id.as_real().unwrap().get()).collect(),
        other => panic!("not an x2many value: {other:?}"),
    }
}

/// A second company with the main company's currency.
pub async fn other_company(tx: &mut Transaction, main: i64, name: &str) -> i64 {
    let currency = records(tx, "res.company", main)
        .cell(tx, "currency_id")
        .await
        .unwrap();
    create(
        tx,
        "res.company",
        named(name).set_cell("currency_id", currency),
    )
    .await
}

/// The values of an internal user of `company` holding `groups` (record ids).
pub fn user(login: &str, company: i64, groups: &[i64]) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(login.into()))
        .set_cell("login", Cell::Str(login.into()))
        .set_cell("company_id", Cell::Id(company))
        .set_commands("company_ids", [Command::Set(vec![real(company)])])
        .set_commands(
            "group_ids",
            [Command::Set(groups.iter().copied().map(real).collect())],
        )
}

/// The stages of `project` as `(name, sequence, folded)`, in board order.
pub async fn stages_of(client: &tokio_postgres::Client, project: i64) -> Vec<(String, i32, bool)> {
    client
        .query(
            "SELECT name::text, sequence, coalesce(fold, false) FROM project_task_type \
             WHERE project_id = $1::int8 ORDER BY sequence, id",
            &[&project],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect()
}

/// The template stages as `(name, sequence, folded)`, from the constant above.
pub fn template_stages() -> Vec<(String, i32, bool)> {
    TEMPLATE_STAGES
        .iter()
        .map(|(_, name, sequence, fold)| ((*name).to_owned(), *sequence, *fold))
        .collect()
}
