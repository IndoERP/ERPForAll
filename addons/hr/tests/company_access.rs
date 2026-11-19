// SPDX-License-Identifier: MIT OR Apache-2.0
//! `hr.job` declares `company_access = direct(field = "company_id", empty = "allow")`: a user
//! who holds only company A reaches, through JSON-2, the positions of A and the positions
//! without a company, and neither finds, reads nor selects the positions of company B.
//! `hr.employee` declares `direct(field = "company_id", empty = "deny")`: the same user reaches
//! the employees of A only.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::{path::Path, sync::Arc};

use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcService};
use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;
use serde_json::{Value, json};

const STACK_SIZE: usize = 16 * 1024 * 1024;
const LOGIN: &str = "job-reader";
const PASSWORD: &str = "job-company-fixture-password";
const ACCESS_ERROR: &str = "erp.exceptions.AccessError";
const NOT_YOURS: &str = "One of the selected companies does not exist or is not one of yours.";
const OUTSIDE: &str = "The record belongs to a company outside your allowed companies.";

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

fn system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
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

/// Installs `hr` and returns an Orm that enforces the installed access rows and memberships,
/// as a serving node does (the module system's own Orm acts as the superuser only).
async fn installed(db: &TestDb) -> Orm {
    let system = system(db);
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let outcome =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("hr").unwrap()]))
            .await
            .unwrap();
    assert!(outcome.post_commit.is_clean());
    let registry = Arc::clone(outcome.orm.registry());
    let source = PolicySource::build(Arc::clone(&registry), db.pool().clone())
        .await
        .unwrap();
    Orm::builder(Arc::clone(&registry), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .resolver(Resolver::for_registry(&registry).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .build()
        .unwrap()
}

async fn base_xid(db: &TestDb, name: &str) -> i64 {
    db.client()
        .await
        .unwrap()
        .query_one(
            "SELECT res_id::int8 FROM ir_model_data WHERE module = 'base' AND name = $1",
            &[&name],
        )
        .await
        .unwrap()
        .get(0)
}

/// The seeded rows: two companies and one position in each, plus one without a company.
struct Seed {
    own_company: i64,
    other_company: i64,
    own_job: i64,
    other_job: i64,
    shared_job: i64,
    own_employee: i64,
    other_employee: i64,
}

impl Seed {
    fn jobs(&self) -> [i64; 3] {
        [self.own_job, self.other_job, self.shared_job]
    }
}

fn real(id: i64) -> RecId {
    RecId::from_real(id).unwrap()
}

/// Seeds the rows and an internal user whose only company is the main one.
async fn seed(db: &TestDb, orm: &Orm) -> Seed {
    let own_company = base_xid(db, "main_company").await;
    let user_group = base_xid(db, "group_user").await;
    let admin_group = base_xid(db, "group_system").await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let companies = tx.env().records("res.company").unwrap();
    let currency = companies
        .browse([own_company])
        .unwrap()
        .cell(&mut tx, "currency_id")
        .await
        .unwrap();
    let other_company = companies
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Other Company".into()))
                    .set_cell("currency_id", currency),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let jobs = tx.env().records("hr.job").unwrap();
    let job = async |tx: &mut erp_orm::Transaction, name: &str, company: Option<i64>| {
        let vals = RawVals::new().set_cell("name", Cell::Str(name.into()));
        let vals = match company {
            Some(id) => vals.set_cell("company_id", Cell::Id(id)),
            None => vals,
        };
        jobs.create(tx, vec![vals]).await.unwrap().ids()[0]
    };
    let own_job = job(&mut tx, "Own Engineer", Some(own_company)).await;
    let other_job = job(&mut tx, "Other Engineer", Some(other_company)).await;
    let shared_job = job(&mut tx, "Shared Engineer", None).await;
    let employees = tx.env().records("hr.employee").unwrap();
    let employee = async |tx: &mut erp_orm::Transaction, name: &str, company: i64| {
        let vals = RawVals::new()
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("company_id", Cell::Id(company));
        employees.create(tx, vec![vals]).await.unwrap().ids()[0]
    };
    let own_employee = employee(&mut tx, "Own Employee", own_company).await;
    let other_employee = employee(&mut tx, "Other Employee", other_company).await;
    // The base addon refuses a user table without an administrator, so one comes first.
    let user = |login: &str, group: i64| {
        RawVals::new()
            .set_cell("name", Cell::Str(login.into()))
            .set_cell("login", Cell::Str(login.into()))
            .set_cell("password", Cell::Str(PASSWORD.into()))
            .set_cell("company_id", Cell::Id(own_company))
            .set_commands("company_ids", [Command::Set(vec![real(own_company)])])
            .set_commands("group_ids", [Command::Set(vec![real(group)])])
    };
    tx.env()
        .records("res.users")
        .unwrap()
        .create(
            &mut tx,
            vec![user("job-admin", admin_group), user(LOGIN, user_group)],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    Seed {
        own_company,
        other_company,
        own_job,
        other_job,
        shared_job,
        own_employee,
        other_employee,
    }
}

async fn principal(auth: &Auth) -> erp_orm::Principal {
    let session = auth
        .authenticate(LoginRequest::new(
            LOGIN,
            PASSWORD,
            ClientAddr::InProcess("hr-company-access-test"),
        ))
        .await
        .unwrap();
    let mut begun = auth
        .begin_session(session.session().secret(), CredentialOptions::new())
        .await
        .unwrap();
    let principal = begun.tx.env().principal().clone();
    begun.tx.rollback().await.unwrap();
    principal
}

/// One JSON-2 call (`POST /json/2/<model>/<method>`) as the reader: its status and body.
async fn call(
    service: &RpcService,
    who: &erp_orm::Principal,
    model: &str,
    method: &str,
    body: &Value,
) -> (u16, Value) {
    let response = service
        .call(RpcCall::json2(
            &format!("/json/2/{model}/{method}"),
            serde_json::to_vec(body).unwrap(),
            Caller::Principal(who.clone()),
        ))
        .await;
    let json = serde_json::from_slice(response.body()).unwrap();
    (response.status(), json)
}

fn sorted_ids(rows: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = rows
        .as_array()
        .expect("search_read answers a list of rows")
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect();
    ids.sort_unstable();
    ids
}

/// The text of an access refusal: a 403 `AccessError`.
fn refusal(status: u16, body: &Value) -> &str {
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["name"], ACCESS_ERROR, "{body}");
    body["message"].as_str().unwrap()
}

/// What one model's case checks: the rows a reader of the own company must find (`reachable`),
/// the row of the other company (`foreign`), and every seeded row (`all`).
struct Case {
    model: &'static str,
    label: &'static str,
    all: Vec<i64>,
    reachable: Vec<i64>,
    foreign: i64,
}

async fn scenario(case: impl Fn(&Seed) -> Case) {
    let db = TestDb::create().await.unwrap();
    let orm = installed(&db).await;
    let seed = seed(&db, &orm).await;
    let case = case(&seed);
    let auth = Auth::new(
        orm.clone(),
        erp_security::auth::AuthConfig::DEFAULT,
        HashPool::global(),
    )
    .await
    .unwrap();
    let reader = principal(&auth).await;
    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
    let all = json!([["id", "in", case.all]]);

    // The rows exist: the superuser sees all of them.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let every = tx
        .env()
        .records(case.model)
        .unwrap()
        .search_count(&mut tx, &erp_sdk::prelude::Domain::TRUE, None)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(every, case.all.len() as u64);

    // A search finds the reachable rows, with the default companies and with the own company
    // asked for explicitly.
    let mut expected = case.reachable.clone();
    expected.sort_unstable();
    for context in [
        json!({}),
        json!({"allowed_company_ids": [seed.own_company]}),
    ] {
        let (status, rows) = call(
            &service,
            &reader,
            case.model,
            "search_read",
            &json!({"domain": all, "fields": ["name"], "context": context}),
        )
        .await;
        assert_eq!(status, 200, "{rows}");
        assert_eq!(sorted_ids(&rows), expected, "{context}");
    }

    // Reading the other company's row by id is refused, not answered with an empty list, and
    // the refusal names the company boundary.
    let (status, body) = call(
        &service,
        &reader,
        case.model,
        "read",
        &json!({"ids": [case.foreign], "fields": ["name"]}),
    )
    .await;
    let message = refusal(status, &body);
    assert!(
        message.starts_with("Access rules protect some of these records.")
            && message.contains(&format!(
                "has no 'read' access to:\n- {} ({})",
                case.label, case.model
            ))
            && message.ends_with(OUTSIDE),
        "{message}"
    );

    // Asking for the other company in the context is refused, alone or next to the own one.
    for companies in [
        vec![seed.other_company],
        vec![seed.own_company, seed.other_company],
    ] {
        let (status, body) = call(
            &service,
            &reader,
            case.model,
            "search_read",
            &json!({"domain": all, "fields": ["name"],
                    "context": {"allowed_company_ids": companies}}),
        )
        .await;
        assert_eq!(refusal(status, &body), NOT_YOURS);
    }
}

fn run(case: fn(&Seed) -> Case) {
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(scenario(case));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_user_of_one_company_cannot_reach_job_positions_of_another() {
    run(|seed| Case {
        model: "hr.job",
        label: "Job Position",
        all: seed.jobs().to_vec(),
        reachable: vec![seed.own_job, seed.shared_job],
        foreign: seed.other_job,
    });
}

#[test]
fn a_user_of_one_company_cannot_reach_employees_of_another() {
    run(|seed| Case {
        model: "hr.employee",
        label: "Employee",
        all: vec![seed.own_employee, seed.other_employee],
        reachable: vec![seed.own_employee],
        foreign: seed.other_employee,
    });
}
