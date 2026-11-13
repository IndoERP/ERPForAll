// SPDX-License-Identifier: MIT OR Apache-2.0
//! Approval policy revisions on an installed `base` with the real access policy (slice
//! T2): default data, who may write, immutability, the write-time refusals, B8's
//! specificity order and B7's eligible set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use erp_addon_base::ir_approval_policy::{
    ApprovalCall, PolicyOutcome, PolicyRevisionId, eligible_approvers, resolve,
};
use erp_fields::{Cell, Error, ErrorKind, RecId, Value};
use erp_modules::{
    AddonIndex,
    system::{InitOptions, ModuleSystem},
};
use erp_orm::{
    Command, CompanyId, Env, Operation, Orm, RawVals, SUPERUSER_ID, Transaction, UserId,
};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

const MODEL: &str = "ir.approval.policy";

/// On a 16 MiB stack: the ORM's create and write chains overflow the default test thread stack in
/// unoptimized builds (the pattern of `base_company_branches.rs`).
fn run<F: std::future::Future<Output = ()>>(test: impl FnOnce() -> F + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
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

fn repo_addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

struct Fixture {
    db: TestDb,
    _system: ModuleSystem,
    /// The installed ORM without the access engine.
    setup: Orm,
    /// An ORM with the real access policy, read when it was built ([`Fixture::refresh`]).
    orm: Orm,
    main: i64,
    group_user: i64,
    group_system: i64,
}

async fn xmlid(tx: &mut Transaction, name: &str) -> i64 {
    tx.query(
        "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name=$1",
        &[&name],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn fixture() -> Fixture {
    let db = TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(repo_addons());
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
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        // every discovered native addon's code is linked; only base is installed
        .code([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    assert!(init.post_commit.is_clean(), "{:?}", init.post_commit);
    assert!(init.data_notices.is_empty(), "{:?}", init.data_notices);
    let setup = init.orm.clone();
    let orm = policy_orm(&db, &setup).await;
    let mut tx = setup.begin(SUPERUSER_ID).await.unwrap();
    let main = xmlid(&mut tx, "main_company").await;
    let group_user = xmlid(&mut tx, "group_user").await;
    let group_system = xmlid(&mut tx, "group_system").await;
    tx.rollback().await.unwrap();
    Fixture {
        db,
        _system: system,
        setup,
        orm,
        main,
        group_user,
        group_system,
    }
}

async fn policy_orm(db: &TestDb, setup: &Orm) -> Orm {
    let source = PolicySource::build(Arc::clone(setup.registry()), db.pool().clone())
        .await
        .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(setup.registry()), db.pool().clone())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(setup.registry()).unwrap())
        .policy(provider)
        // the impersonation case switches an administrator to the clerk (A4)
        .impersonate_capability(&erp_security::principal::PRINCIPAL_IMPERSONATE)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}

/// (xml id, noupdate, active, all companies, kind, effect, wildcard, outcome, approvers)
type DefaultRow = (String, bool, bool, bool, String, String, bool, String, i64);

fn ids(ids: &[i64]) -> Vec<RecId> {
    ids.iter()
        .map(|id| RecId::from_real(*id).unwrap())
        .collect()
}

impl Fixture {
    /// An internal user in `companies` (the first is its default) holding `groups`.
    async fn user(&self, login: &str, companies: &[i64], groups: &[i64]) -> i64 {
        let mut tx = self.setup.begin(SUPERUSER_ID).await.unwrap();
        let created = tx
            .env()
            .records("res.users")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(login.into()))
                        .set_cell("login", Cell::Str(login.into()))
                        .set_cell("company_id", Cell::Id(companies[0]))
                        .set_commands("company_ids", [Command::Set(ids(companies))])
                        .set_commands("group_ids", [Command::Set(ids(groups))]),
                ],
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        created.ids()[0]
    }

    async fn record(&self, model: &str, vals: RawVals) -> i64 {
        let mut tx = self.setup.begin(SUPERUSER_ID).await.unwrap();
        let created = tx
            .env()
            .records(model)
            .unwrap()
            .create(&mut tx, vec![vals])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        created.ids()[0]
    }

    /// A company with the main company's currency.
    async fn company(&self, name: &str) -> i64 {
        let currency: i64 = self
            .db
            .client()
            .await
            .unwrap()
            .query_one(
                "SELECT currency_id::int8 FROM res_company WHERE id = $1",
                &[&self.main],
            )
            .await
            .unwrap()
            .get(0);
        self.record(
            "res.company",
            RawVals::new()
                .set_cell("name", Cell::Str(name.into()))
                .set_cell("currency_id", Cell::Id(currency)),
        )
        .await
    }

    async fn sql(&self, sql: &str, params: &[&(dyn tokio_postgres::types::ToSql + Sync)]) {
        self.db
            .client()
            .await
            .unwrap()
            .execute(sql, params)
            .await
            .unwrap();
    }

    /// A new access policy that sees the groups and users created since the last one.
    async fn refresh(&mut self) {
        self.orm = policy_orm(&self.db, &self.setup).await;
    }

    async fn begin(&self, uid: i64) -> Transaction {
        self.orm.begin(UserId::new(uid).unwrap()).await.unwrap()
    }

    /// `vals` created by `uid` in a transaction of its own, committed when accepted.
    async fn create(&self, uid: i64, vals: RawVals) -> Result<i64, Error> {
        let mut tx = self.begin(uid).await;
        let created = tx
            .env()
            .records(MODEL)
            .unwrap()
            .create(&mut tx, vec![vals])
            .await;
        match created {
            Ok(created) => {
                tx.commit().await.unwrap();
                Ok(created.ids()[0])
            }
            Err(error) => {
                tx.rollback().await.unwrap();
                Err(error)
            }
        }
    }

    /// An active user Approve or Deny revision for the tests of resolution: the model refuses to
    /// create one (nothing consults policies yet), so it is created archived and
    /// activated by SQL. These tests therefore no longer prove that a model-admitted active
    /// revision resolves; the slice that removes the fence (and
    /// ) restores them by creating the revisions active again.
    async fn create_unfenced(&self, uid: i64, vals: RawVals) -> Result<i64, Error> {
        let id = self
            .create(uid, vals.set_cell("active", Cell::Bool(false)))
            .await?;
        self.sql(
            "UPDATE ir_approval_policy SET active = true WHERE id = $1",
            &[&id],
        )
        .await;
        Ok(id)
    }

    async fn write(&self, uid: i64, id: i64, vals: RawVals) -> Result<(), Error> {
        let mut tx = self.begin(uid).await;
        let records = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
        let written = records.write(&mut tx, vals).await;
        match written {
            Ok(()) => tx.commit().await.map_err(|e| panic!("{e}")),
            Err(error) => {
                tx.rollback().await.unwrap();
                Err(error)
            }
        }
    }

    async fn unlink(&self, uid: i64, id: i64) -> Result<(), Error> {
        let mut tx = self.begin(uid).await;
        let records = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
        let unlinked = records.unlink(&mut tx).await;
        tx.rollback().await.unwrap();
        unlinked
    }

    async fn archive(&self, uid: i64, id: i64) {
        self.write(
            uid,
            id,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    }

    /// `call` resolved for `uid` with the active companies `companies` (its default when empty).
    async fn resolve(
        &self,
        uid: i64,
        companies: &[i64],
        call: &ApprovalCall,
    ) -> Result<PolicyOutcome, Error> {
        let mut tx = self.begin(uid).await;
        let env = with_companies(&tx.env(), companies);
        let outcome = resolve(&env, &mut tx, call).await;
        tx.rollback().await.unwrap();
        outcome
    }
}

fn with_companies(env: &Env, companies: &[i64]) -> Env {
    if companies.is_empty() {
        return env.clone();
    }
    env.with_context([(
        "allowed_company_ids",
        Value::list(companies.iter().map(|c| Value::Int(*c))),
    )])
}

/// A revision for users.
fn policy(name: &str, effect: &str, outcome: &str) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("principal_kind", Cell::Str("user".into()))
        .set_cell("effect", Cell::Str(effect.into()))
        .set_cell("outcome", Cell::Str(outcome.into()))
}

fn select(
    vals: RawVals,
    company: Option<i64>,
    model: Option<&str>,
    method: Option<&str>,
) -> RawVals {
    let mut vals = vals;
    if let Some(company) = company {
        vals = vals.set_cell("company_id", Cell::Id(company));
    }
    if let Some(model) = model {
        vals = vals.set_cell("model", Cell::Str(model.into()));
    }
    if let Some(method) = method {
        vals = vals.set_cell("method", Cell::Str(method.into()));
    }
    vals
}

fn approvers(vals: RawVals, users: &[i64], groups: &[i64]) -> RawVals {
    let mut vals = vals;
    if !users.is_empty() {
        vals = vals.set_commands("approver_user_ids", [Command::Set(ids(users))]);
    }
    if !groups.is_empty() {
        vals = vals.set_commands("approver_group_ids", [Command::Set(ids(groups))]);
    }
    vals
}

/// `res.partner.write` on one partner of `company` (not widened: active companies plus `company`).
fn partner_write(company: i64) -> ApprovalCall {
    ApprovalCall::on_records("res.partner", "write", &[Some(company_id(company))]).unwrap()
}

fn company_id(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

fn revision(id: i64) -> PolicyRevisionId {
    PolicyRevisionId::new(id).unwrap()
}

fn user_ids(ids: &[i64]) -> Vec<UserId> {
    ids.iter().map(|id| UserId::new(*id).unwrap()).collect()
}

fn approved_by(outcome: PolicyOutcome) -> (PolicyRevisionId, Vec<UserId>) {
    match outcome {
        PolicyOutcome::Approve {
            revision,
            approvers,
        } => (revision, approvers.ids().to_vec()),
        other => panic!("expected Approve, got {other:?}"),
    }
}

fn validation(error: &Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    assert_eq!(error.message(), message);
}

fn refused(id: i64, name: &str, reason: &str) -> String {
    format!("Approval policy revision '{name}' (id {id}): {reason}")
}

/// with their accepted parts)
#[test]
fn default_revisions_are_inert_agent_approvals_and_no_user_revision() {
    run(default_revisions_are_inert_agent_approvals_and_no_user_revision_body);
}

async fn default_revisions_are_inert_agent_approvals_and_no_user_revision_body() {
    let f = fixture().await;
    let client = f.db.client().await.unwrap();
    let rows = client
        .query(
            "SELECT d.name, d.noupdate, p.active, p.company_id IS NULL, p.principal_kind, p.effect, \
             p.model IS NULL AND p.method IS NULL, p.outcome, \
             (SELECT count(*) FROM ir_approval_policy_users_rel r WHERE r.policy_id = p.id) \
             + (SELECT count(*) FROM ir_approval_policy_groups_rel g WHERE g.policy_id = p.id) \
             FROM ir_model_data d JOIN ir_approval_policy p ON p.id = d.res_id \
             WHERE d.module = 'base' AND d.model = $1 ORDER BY d.name",
            &[&MODEL],
        )
        .await
        .unwrap();
    let got: Vec<DefaultRow> = rows
        .iter()
        .map(|r| {
            (
                r.get(0),
                r.get(1),
                r.get(2),
                r.get(3),
                r.get(4),
                r.get(5),
                r.get(6),
                r.get(7),
                r.get(8),
            )
        })
        .collect();
    let expect = |name: &str, effect: &str| {
        (
            name.to_owned(),
            true,
            true,
            true,
            "agent".to_owned(),
            effect.to_owned(),
            true,
            "approve".to_owned(),
            0,
        )
    };
    assert_eq!(
        got,
        vec![
            expect("approval_policy_agent_destructive", "destructive"),
            expect("approval_policy_agent_external", "external"),
        ]
    );
    let total: i64 = client
        .query_one("SELECT count(*) FROM ir_approval_policy", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(total, 2, "base ships no user revision");

    // A user call is Unmanaged on a fresh database: the agent defaults never select it.
    let admin = f
        .user("policy-admin", &[f.main], &[f.group_user, f.group_system])
        .await;
    let delete =
        ApprovalCall::on_records("res.partner", "unlink", &[Some(company_id(f.main))]).unwrap();
    assert_eq!(
        f.resolve(admin, &[], &delete).await.unwrap(),
        PolicyOutcome::Unmanaged
    );
    // System never passes the approval phase.
    let mut tx = f.orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let error = resolve(&env, &mut tx, &delete).await.unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(error.kind(), ErrorKind::Programming, "{error}");
    assert_eq!(
        error.message(),
        "Approval policies apply to user principals only; a system call does not pass the \
         approval phase."
    );
    let empty = ApprovalCall::model_level("", "write").unwrap_err();
    validation(
        &empty,
        "An approval call names a model or method that is empty, longer than 256 bytes or holds \
         a NUL.",
    );
    drop(client);
    f.db.cleanup().await.unwrap();
}

/// levels, exact company and all companies)
#[test]
fn the_most_specific_revision_wins_for_every_pair_of_levels() {
    run(the_most_specific_revision_wins_for_every_pair_of_levels_body);
}

async fn the_most_specific_revision_wins_for_every_pair_of_levels_body() {
    let f = fixture().await;
    let admin = f
        .user("policy-admin", &[f.main], &[f.group_user, f.group_system])
        .await;
    // Most specific first (B8): exact company before all companies, then method, model, wildcard.
    let levels: [(Option<i64>, Option<&str>, Option<&str>); 6] = [
        (Some(f.main), Some("res.partner"), Some("write")),
        (Some(f.main), Some("res.partner"), None),
        (Some(f.main), None, None),
        (None, Some("res.partner"), Some("write")),
        (None, Some("res.partner"), None),
        (None, None, None),
    ];
    let call = partner_write(f.main);
    let mut pairs = 0;
    for (i, strong) in levels.iter().enumerate() {
        for weak in &levels[i + 1..] {
            // written in both orders, so row order cannot decide
            for weak_first in [false, true] {
                let make =
                    |name: &str, level: &(Option<i64>, Option<&str>, Option<&str>), outcome| {
                        select(policy(name, "write", outcome), level.0, level.1, level.2)
                    };
                let (first, second) = if weak_first {
                    (make("weak", weak, "deny"), make("strong", strong, "allow"))
                } else {
                    (make("strong", strong, "allow"), make("weak", weak, "deny"))
                };
                let a = f.create_unfenced(admin, first).await.unwrap();
                let b = f.create_unfenced(admin, second).await.unwrap();
                let strong_id = if weak_first { b } else { a };
                assert_eq!(
                    f.resolve(admin, &[], &call).await.unwrap(),
                    PolicyOutcome::Allow {
                        revision: revision(strong_id)
                    },
                    "{strong:?} must win over {weak:?}"
                );
                // the weaker one alone is selected once the stronger is archived
                f.archive(admin, strong_id).await;
                let weak_id = if weak_first { a } else { b };
                assert_eq!(
                    f.resolve(admin, &[], &call).await.unwrap(),
                    PolicyOutcome::Deny {
                        revision: revision(weak_id)
                    }
                );
                f.archive(admin, weak_id).await;
            }
            pairs += 1;
        }
    }
    assert_eq!(pairs, 15);
    // Another model's or method's revision never selects the call.
    let other = f
        .create_unfenced(
            admin,
            select(
                policy("other", "write", "deny"),
                None,
                Some("res.partner"),
                Some("create"),
            ),
        )
        .await
        .unwrap();
    let elsewhere = f
        .create_unfenced(
            admin,
            select(
                policy("elsewhere", "write", "deny"),
                None,
                Some("res.users"),
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        f.resolve(admin, &[], &call).await.unwrap(),
        PolicyOutcome::Unmanaged
    );
    let create = ApprovalCall::create("res.partner", "create", &[]).unwrap();
    assert_eq!(
        f.resolve(admin, &[], &create).await.unwrap(),
        PolicyOutcome::Deny {
            revision: revision(other)
        }
    );
    // A revision of another effect never selects the call either.
    let destructive = f
        .create_unfenced(
            admin,
            select(policy("delete", "destructive", "deny"), None, None, None),
        )
        .await
        .unwrap();
    assert_eq!(
        f.resolve(admin, &[], &call).await.unwrap(),
        PolicyOutcome::Unmanaged
    );
    let _ = (elsewhere, destructive);
    f.db.cleanup().await.unwrap();
}

/// the database index refuses it too, and active companies must agree)
#[test]
fn equal_selectors_are_refused_and_companies_must_agree() {
    run(equal_selectors_are_refused_and_companies_must_agree_body);
}

async fn equal_selectors_are_refused_and_companies_must_agree_body() {
    let f = fixture().await;
    let second = f.company("Second Company").await;
    let admin = f
        .user(
            "policy-admin",
            &[f.main, second],
            &[f.group_user, f.group_system],
        )
        .await;
    let first = f
        .create_unfenced(
            admin,
            select(
                policy("first", "write", "deny"),
                Some(f.main),
                Some("res.partner"),
                None,
            ),
        )
        .await
        .unwrap();
    let error = f
        .create(
            admin,
            select(
                policy("again", "write", "allow"),
                Some(f.main),
                Some("res.partner"),
                None,
            ),
        )
        .await
        .unwrap_err();
    validation(
        &error,
        &format!(
            "The new approval policy revision 'again' has the same selector as the active \
             revision 'first' (id {first}); archive one of them first."
        ),
    );
    // Two new revisions of one call with the same selector name each other.
    let mut tx = f.begin(admin).await;
    let twins = |name: &str| select(policy(name, "write", "deny"), None, None, None);
    let error = tx
        .env()
        .records(MODEL)
        .unwrap()
        .create(&mut tx, vec![twins("twin a"), twins("twin b")])
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    validation(
        &error,
        "The new approval policy revisions 'twin a' and 'twin b' have the same selector.",
    );
    // An archived revision with the same selector is history, not a tie; reactivating it is refused.
    f.archive(admin, first).await;
    let replaced = f
        .create(
            admin,
            select(
                policy("replaced", "write", "allow"),
                Some(f.main),
                Some("res.partner"),
                None,
            ),
        )
        .await
        .unwrap();
    let error = f
        .write(
            admin,
            first,
            RawVals::new().set_cell("active", Cell::Bool(true)),
        )
        .await
        .unwrap_err();
    validation(
        &error,
        &format!(
            "Approval policy revision 'first' (id {first}) has the same selector as the active \
             revision 'replaced' (id {replaced}); archive one of them first."
        ),
    );
    // the archive state takes no other form than true or false
    let error = f
        .write(
            admin,
            first,
            RawVals::new().set_cell("active", Cell::Int(1)),
        )
        .await
        .unwrap_err();
    validation(
        &error,
        "The archive state of an approval policy revision is true or false.",
    );
    // The partial unique index is the arbiter between concurrent writers.
    let client = f.db.client().await.unwrap();
    let raw = client
        .execute(
            "INSERT INTO ir_approval_policy (name, active, company_id, principal_kind, effect, \
             model, outcome) VALUES ('raw', true, $1, 'user', 'write', 'res.partner', 'deny')",
            &[&f.main],
        )
        .await
        .unwrap_err();
    assert_eq!(
        raw.code(),
        Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION)
    );

    // Two active companies selecting different revisions are refused, not resolved by order.
    let call = partner_write(second);
    let error = f
        .resolve(admin, &[f.main, second], &call)
        .await
        .unwrap_err();
    validation(
        &error,
        &format!(
            "The companies of this call select different approval policy revisions \
             (company {}: revision {replaced}, company {second}: no revision); call with one \
             active company.",
            f.main
        ),
    );
    assert_eq!(
        f.resolve(admin, &[second], &call).await.unwrap(),
        PolicyOutcome::Unmanaged
    );
    let everywhere = f
        .create(
            admin,
            select(
                policy("everywhere", "write", "allow"),
                None,
                Some("res.partner"),
                Some("write"),
            ),
        )
        .await
        .unwrap();
    let error = f
        .resolve(admin, &[f.main, second], &call)
        .await
        .unwrap_err();
    validation(
        &error,
        &format!(
            "The companies of this call select different approval policy revisions \
             (company {}: revision {replaced}, company {second}: revision {everywhere}); call \
             with one active company.",
            f.main
        ),
    );
    f.archive(admin, replaced).await;
    assert_eq!(
        f.resolve(admin, &[f.main, second], &call).await.unwrap(),
        PolicyOutcome::Allow {
            revision: revision(everywhere)
        }
    );
    drop(client);
    f.db.cleanup().await.unwrap();
}

/// changed or deleted, only archived; the write-time refusals of the minimum delivery)
#[test]
fn only_administrators_write_and_revisions_are_immutable() {
    run(only_administrators_write_and_revisions_are_immutable_body);
}

async fn only_administrators_write_and_revisions_are_immutable_body() {
    let f = fixture().await;
    let admin = f
        .user("policy-admin", &[f.main], &[f.group_user, f.group_system])
        .await;
    let clerk = f.user("policy-clerk", &[f.main], &[f.group_user]).await;
    let approver = f.user("policy-approver", &[f.main], &[f.group_user]).await;
    let spec = f.orm.registry().model(MODEL).unwrap();
    let denied = |op| {
        erp_security::explain::model_message(spec, op, &["Settings Administration".to_owned()])
    };

    let error = f
        .create(clerk, policy("clerk", "write", "deny"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    assert_eq!(error.message(), denied(Operation::Create));

    let id = f
        .create_unfenced(
            admin,
            approvers(
                select(
                    policy("guarded", "write", "approve"),
                    None,
                    Some("res.partner"),
                    None,
                ),
                &[approver],
                &[],
            ),
        )
        .await
        .unwrap();
    let error = f
        .write(
            clerk,
            id,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    assert_eq!(error.message(), denied(Operation::Write));
    let error = f.unlink(clerk, id).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    assert_eq!(error.message(), denied(Operation::Unlink));

    let immutable =
        "Approval policy revisions cannot be changed; archive this revision and create a new one.";
    for vals in [
        RawVals::new().set_cell("name", Cell::Str("renamed".into())),
        RawVals::new().set_cell("outcome", Cell::Str("deny".into())),
        RawVals::new()
            .set_cell("active", Cell::Bool(true))
            .set_cell("method", Cell::Str("write".into())),
        RawVals::new().set_commands("approver_user_ids", [Command::Set(vec![])]),
    ] {
        let error = f.write(admin, id, vals).await.unwrap_err();
        validation(&error, immutable);
    }
    let error = f.unlink(admin, id).await.unwrap_err();
    validation(
        &error,
        "Approval policy revisions cannot be deleted; archive them instead.",
    );
    f.archive(admin, id).await;
    // reactivating a user Approve revision is fenced; SQL restores the old state
    let error = f
        .write(
            admin,
            id,
            RawVals::new().set_cell("active", Cell::Bool(true)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    f.sql(
        "UPDATE ir_approval_policy SET active = true WHERE id = $1",
        &[&id],
    )
    .await;

    // The write-time refusals: each names the revision, nothing persists.
    let before: i64 =
        f.db.client()
            .await
            .unwrap()
            .query_one("SELECT max(id)::int8 FROM ir_approval_policy", &[])
            .await
            .unwrap()
            .get(0);
    let cases: Vec<(RawVals, &str, String)> = vec![
        (
            policy("no approver", "write", "approve"),
            "no approver",
            "requires approval but names no approver or approver group.".to_owned(),
        ),
        (
            approvers(policy("read", "read", "approve"), &[approver], &[]),
            "read",
            "requiring approval for read calls is not supported.".to_owned(),
        ),
        (
            approvers(policy("external", "external", "approve"), &[approver], &[]),
            "external",
            "requiring approval for external calls of users is not supported until \
             external-effect admission is accepted."
                .to_owned(),
        ),
        (
            policy("agent allow", "write", "allow")
                .set_cell("principal_kind", Cell::Str("agent".into())),
            "agent allow",
            "an Allow exception for agent and service principals is not supported until ADR \
             0169 accepts it (D2, a later release)."
                .to_owned(),
        ),
        (
            select(
                policy("typo", "write", "deny"),
                None,
                Some("res.partnr"),
                None,
            ),
            "typo",
            "names the unknown model 'res.partnr'.".to_owned(),
        ),
        (
            select(
                policy("effect", "write", "deny"),
                None,
                Some("res.partner"),
                Some("unlink"),
            ),
            "effect",
            "'res.partner.unlink' is not an RPC method of effect write.".to_owned(),
        ),
        (
            approvers(policy("allow named", "write", "allow"), &[approver], &[]),
            "allow named",
            "only an Approve revision names approvers.".to_owned(),
        ),
        (
            approvers(
                select(
                    policy("self", "write", "approve"),
                    None,
                    Some("res.users"),
                    None,
                ),
                &[SUPERUSER_ID.get()],
                &[],
            ),
            "self",
            format!(
                "user {} is not an active internal user and cannot approve.",
                SUPERUSER_ID.get()
            ),
        ),
    ];
    for (vals, name, reason) in cases {
        let error = f.create(admin, vals).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{name}: {error}");
        let message = error.message();
        let id = message
            .strip_prefix(&format!("Approval policy revision '{name}' (id "))
            .and_then(|rest| rest.split_once("): "))
            .unwrap_or_else(|| panic!("{message}"));
        assert_eq!(id.1, reason, "{name}");
        assert!(id.0.parse::<i64>().unwrap() > before, "{message}");
    }
    let after: i64 =
        f.db.client()
            .await
            .unwrap()
            .query_one("SELECT count(*) FROM ir_approval_policy", &[])
            .await
            .unwrap()
            .get(0);
    assert_eq!(
        after, 3,
        "two defaults and 'guarded'; no refused revision persisted"
    );
    // A method selector without a model is refused by the database as well.
    let error = f
        .create(
            admin,
            select(
                policy("method only", "write", "deny"),
                None,
                None,
                Some("write"),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    f.db.cleanup().await.unwrap();
}

/// excluded by `res.users` id (B7); archived, share and other-company users never eligible; group
/// members read now, through implied groups)
#[test]
fn eligible_approvers_exclude_the_requester_and_follow_current_membership() {
    run(eligible_approvers_exclude_the_requester_and_follow_current_membership_body);
}

async fn eligible_approvers_exclude_the_requester_and_follow_current_membership_body() {
    let mut f = fixture().await;
    let second = f.company("Second Company").await;
    let admin = f
        .user(
            "policy-admin",
            &[f.main, second],
            &[f.group_user, f.group_system],
        )
        .await;
    let deciders = f
        .record(
            "res.groups",
            RawVals::new().set_cell("name", Cell::Str("Partner Deciders".into())),
        )
        .await;
    let seniors = f
        .record(
            "res.groups",
            RawVals::new()
                .set_cell("name", Cell::Str("Senior Deciders".into()))
                .set_commands("implied_ids", [Command::Set(ids(&[deciders]))]),
        )
        .await;
    let named = f.user("named", &[f.main, second], &[f.group_user]).await;
    let member = f
        .user("member", &[f.main, second], &[f.group_user, deciders])
        .await;
    let senior = f
        .user("senior", &[f.main, second], &[f.group_user, seniors])
        .await;
    let main_only = f
        .user("main-only", &[f.main], &[f.group_user, deciders])
        .await;
    let clerk = f
        .user("clerk", &[f.main, second], &[f.group_user, deciders])
        .await;

    f.refresh().await;

    // A named archived user is refused at write time.
    f.sql(
        "UPDATE res_users SET active = false WHERE id = $1",
        &[&main_only],
    )
    .await;
    let error = f
        .create(
            admin,
            approvers(policy("archived", "write", "approve"), &[main_only], &[]),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    assert!(
        error.message().ends_with(&format!(
            "user {main_only} is not an active internal user and cannot approve."
        )),
        "{error}"
    );
    f.sql(
        "UPDATE res_users SET active = true WHERE id = $1",
        &[&main_only],
    )
    .await;

    let id = f
        .create_unfenced(
            admin,
            approvers(
                select(
                    policy("partners", "write", "approve"),
                    None,
                    Some("res.partner"),
                    None,
                ),
                &[admin, named],
                &[deciders],
            ),
        )
        .await
        .unwrap();
    let call = partner_write(f.main);
    // The requester (admin, also named) is never eligible; main-only is out in the second company.
    let (got, eligible) = approved_by(f.resolve(admin, &[f.main, second], &call).await.unwrap());
    assert_eq!(got, revision(id));
    assert_eq!(eligible, user_ids(&[named, member, senior, clerk]));
    let (_, eligible) = approved_by(f.resolve(admin, &[f.main], &call).await.unwrap());
    assert_eq!(
        eligible,
        user_ids(&[named, member, senior, main_only, clerk])
    );
    // The clerk requests: it is not in its own eligible set, admin is.
    let (_, eligible) = approved_by(f.resolve(clerk, &[f.main], &call).await.unwrap());
    assert_eq!(
        eligible,
        user_ids(&[admin, named, member, senior, main_only])
    );

    // Impersonation: neither the subject nor the impersonating user may decide.
    let mut tx = f.begin(admin).await;
    let env = with_companies(&tx.env().with_user(clerk), &[f.main]);
    let outcome = resolve(&env, &mut tx, &call).await.unwrap();
    tx.rollback().await.unwrap();
    let (_, eligible) = approved_by(outcome);
    assert_eq!(eligible, user_ids(&[named, member, senior, main_only]));

    // Archived and share users drop out; membership is read when resolve runs.
    f.sql(
        "UPDATE res_users SET active = false WHERE id = $1",
        &[&named],
    )
    .await;
    f.sql(
        "UPDATE res_users SET share = true WHERE id = $1",
        &[&senior],
    )
    .await;
    f.sql(
        "DELETE FROM res_groups_users_rel WHERE uid = $1 AND gid = $2",
        &[&member, &deciders],
    )
    .await;
    let (_, eligible) = approved_by(f.resolve(admin, &[f.main], &call).await.unwrap());
    assert_eq!(eligible, user_ids(&[main_only, clerk]));
    let mut tx = f.orm.begin(SUPERUSER_ID).await.unwrap();
    let direct = eligible_approvers(
        &mut tx,
        revision(id),
        &user_ids(&[clerk]),
        &[company_id(f.main)],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(direct.ids(), user_ids(&[admin, main_only]));
    assert!(direct.contains(UserId::new(admin).unwrap()));
    assert!(!direct.contains(UserId::new(clerk).unwrap()));

    // With nobody else left, the call is refused before any request could persist (D2, D9).
    f.sql(
        "DELETE FROM res_groups_users_rel WHERE gid = $1",
        &[&deciders],
    )
    .await;
    let error = f.resolve(admin, &[f.main], &call).await.unwrap_err();
    validation(
        &error,
        &refused(
            id,
            "partners",
            "no eligible approver other than the requester is configured for this call (ADR \
             0169 D2).",
        ),
    );
    let mut tx = f.orm.begin(SUPERUSER_ID).await.unwrap();
    let error = eligible_approvers(&mut tx, revision(id + 1000), &[], &[company_id(f.main)])
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    validation(
        &error,
        &format!("Approval policy revision {} does not exist.", id + 1000),
    );
    f.db.cleanup().await.unwrap();
}

/// The refusal a requester without read access to the revisions gets (review r1, D-c).
const UNAPPROVABLE: &str =
    "This action needs an approval that is not set up correctly; ask an administrator.";

/// administrator outside company B neither reads, creates nor archives B's revisions; an
/// all-companies revision stays global administration)
#[test]
fn administrators_only_manage_the_revisions_of_their_companies() {
    run(administrators_only_manage_the_revisions_of_their_companies_body);
}

async fn administrators_only_manage_the_revisions_of_their_companies_body() {
    let mut f = fixture().await;
    let other = f.company("Company B").await;
    let admin_a = f
        .user("admin-a", &[f.main], &[f.group_user, f.group_system])
        .await;
    let admin_b = f
        .user("admin-b", &[other], &[f.group_user, f.group_system])
        .await;
    let approver_b = f.user("approver-b", &[other], &[f.group_user]).await;
    let admin_ab = f
        .user(
            "admin-ab",
            &[f.main, other],
            &[f.group_user, f.group_system],
        )
        .await;
    let deciders_b = f
        .record(
            "res.groups",
            RawVals::new().set_cell("name", Cell::Str("B Deciders".into())),
        )
        .await;
    f.refresh().await;
    let of_b = |name: &str, outcome| {
        select(
            policy(name, "write", outcome),
            Some(other),
            Some("res.partner"),
            None,
        )
    };
    let b_deny = f
        .create_unfenced(
            admin_b,
            approvers(of_b("b approve", "approve"), &[approver_b], &[deciders_b]),
        )
        .await
        .unwrap();

    // creating a revision of company B
    let error = f
        .create(admin_a, of_b("a in b", "allow"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    // archiving B's revision
    let error = f
        .write(
            admin_a,
            b_deny,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    // reading B's revisions and their approvers
    let mut tx = f.begin(admin_a).await;
    let seen = tx
        .env()
        .records(MODEL)
        .unwrap()
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    assert!(!seen.ids().contains(&b_deny), "{:?}", seen.ids());
    tx.rollback().await.unwrap();
    let mut tx = f.begin(admin_a).await;
    let error = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse([b_deny])
        .unwrap()
        .cells(&mut tx, "approver_user_ids")
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    // its own company and all companies stay open to it
    let a_own = f
        .create_unfenced(
            admin_a,
            select(
                policy("a own", "write", "deny"),
                Some(f.main),
                Some("res.partner"),
                None,
            ),
        )
        .await
        .unwrap();
    f.create_unfenced(
        admin_a,
        select(
            policy("everywhere", "destructive", "deny"),
            None,
            None,
            None,
        ),
    )
    .await
    .unwrap();
    // B's revision is untouched
    let outcome = f
        .resolve(admin_b, &[], &partner_write(other))
        .await
        .unwrap();
    let (got, eligible) = approved_by(outcome);
    assert_eq!((got, eligible), (revision(b_deny), user_ids(&[approver_b])));
    // its only approver requests: refused without naming the revision it cannot read (D-c)
    let error = f
        .resolve(approver_b, &[], &partner_write(other))
        .await
        .unwrap_err();
    validation(&error, UNAPPROVABLE);

    // D-d: a caller active in A only, calling on a record of company B, meets B's revision
    let error = f
        .resolve(admin_a, &[], &partner_write(other))
        .await
        .unwrap_err();
    validation(&error, UNAPPROVABLE);
    // an administrator of both companies, who may read both revisions, sees why
    let error = f
        .resolve(admin_ab, &[f.main, other], &partner_write(f.main))
        .await
        .unwrap_err();
    validation(
        &error,
        &format!(
            "The companies of this call select different approval policy revisions (company {}: \
             revision {a_own}, company {other}: revision {b_deny}); call with one active company.",
            f.main
        ),
    );

    // S5: the named user, the named group and the company of a revision cannot be deleted
    let client = f.db.client().await.unwrap();
    let rel_rows = || async {
        let row = f
            .db
            .client()
            .await
            .unwrap()
            .query_one(
                "SELECT (SELECT count(*) FROM ir_approval_policy_users_rel WHERE policy_id = $1) \
                 + (SELECT count(*) FROM ir_approval_policy_groups_rel WHERE policy_id = $1)",
                &[&b_deny],
            )
            .await
            .unwrap();
        row.get::<_, i64>(0)
    };
    assert_eq!(rel_rows().await, 2);
    for (sql, id, table) in [
        (
            "DELETE FROM res_users WHERE id = $1",
            approver_b,
            "ir_approval_policy_users_rel",
        ),
        (
            "DELETE FROM res_groups WHERE id = $1",
            deciders_b,
            "ir_approval_policy_groups_rel",
        ),
    ] {
        let error = client.execute(sql, &[&id]).await.unwrap_err();
        let db = error.as_db_error().unwrap_or_else(|| panic!("{error}"));
        assert_eq!(
            (db.code(), db.table()),
            (
                &tokio_postgres::error::SqlState::RESTRICT_VIOLATION,
                Some(table)
            ),
            "{sql}: {db}"
        );
    }
    let error = client
        .execute("DELETE FROM res_company WHERE id = $1", &[&other])
        .await
        .unwrap_err();
    let db = error.as_db_error().unwrap_or_else(|| panic!("{error}"));
    // whichever referencing table PostgreSQL checks first refuses it
    assert!(
        [
            tokio_postgres::error::SqlState::FOREIGN_KEY_VIOLATION,
            tokio_postgres::error::SqlState::RESTRICT_VIOLATION
        ]
        .contains(db.code()),
        "{db}"
    );
    // other tables reference the company too: the revision's own key is RESTRICT
    let on_delete: String = client
        .query_one(
            "SELECT c.confdeltype::text FROM pg_constraint c JOIN pg_attribute a \
             ON a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey) \
             WHERE c.conrelid = 'ir_approval_policy'::regclass AND c.contype = 'f' \
             AND a.attname = 'company_id'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(on_delete, "r");
    assert_eq!(rel_rows().await, 2);
    drop(client);
    f.db.cleanup().await.unwrap();
}

#[test]
fn the_effect_of_a_call_comes_from_the_registry() {
    run(the_effect_of_a_call_comes_from_the_registry_body);
}

async fn the_effect_of_a_call_comes_from_the_registry_body() {
    let mut f = fixture().await;
    let admin = f
        .user("policy-admin", &[f.main], &[f.group_user, f.group_system])
        .await;
    f.refresh().await;
    let deny = f
        .create_unfenced(
            admin,
            select(
                policy("no deletes", "destructive", "deny"),
                None,
                None,
                None,
            ),
        )
        .await
        .unwrap();
    // the call names no effect: `unlink` meets the destructive revision
    let call =
        ApprovalCall::on_records("res.partner", "unlink", &[Some(company_id(f.main))]).unwrap();
    assert_eq!(
        f.resolve(admin, &[], &call).await.unwrap(),
        PolicyOutcome::Deny {
            revision: revision(deny)
        }
    );
    f.db.cleanup().await.unwrap();
}

/// The refusal of a widened call whose companies select different revisions.
fn widened_conflict(low: i64, low_choice: &str, high: i64, high_choice: &str) -> String {
    format!(
        "The companies of this call select different approval policy revisions (company {low}: \
         {low_choice}, company {high}: {high_choice}); this call is resolved for every company \
         you are allowed in, so these revisions must agree."
    )
}

/// are resolved for every company the caller is allowed in, so choosing the active company cannot
/// pass over another company's revision (amendment 2026-11-13); a call on records that
/// all carry a company keeps the active companies plus theirs)
#[test]
fn a_widened_call_meets_the_revisions_of_every_allowed_company() {
    run(a_widened_call_meets_the_revisions_of_every_allowed_company_body);
}

async fn a_widened_call_meets_the_revisions_of_every_allowed_company_body() {
    let mut f = fixture().await;
    let b = f.main;
    let a = f.company("Company A").await;
    let c = f.company("Company C").await;
    assert!(
        b < a && a < c,
        "the messages below name the companies in id order"
    );
    let admin_a = f
        .user("admin-a", &[a], &[f.group_user, f.group_system])
        .await;
    let admin_c = f
        .user("admin-c", &[c], &[f.group_user, f.group_system])
        .await;
    // allowed in A and B, and may not read approval policy revisions
    let clerk_ab = f.user("clerk-ab", &[b, a], &[f.group_user]).await;
    let admin_ab = f
        .user("admin-ab", &[b, a], &[f.group_user, f.group_system])
        .await;
    f.refresh().await;
    // A refuses every write-effect call on partners; B has no revision
    let a_deny = f
        .create_unfenced(
            admin_a,
            select(
                policy("a deny", "write", "deny"),
                Some(a),
                Some("res.partner"),
                None,
            ),
        )
        .await
        .unwrap();
    let a_conflict = widened_conflict(b, "no revision", a, &format!("revision {a_deny}"));

    let create = ApprovalCall::create("res.partner", "create", &[]).unwrap();
    let uncompanied = ApprovalCall::on_records("res.partner", "write", &[None]).unwrap();
    let mixed =
        ApprovalCall::on_records("res.partner", "write", &[Some(company_id(b)), None]).unwrap();
    let none = ApprovalCall::on_records("res.partner", "write", &[]).unwrap();
    let model_level = ApprovalCall::model_level("res.partner", "name_create").unwrap();
    for (name, call) in [
        ("create", &create),
        ("record without a company", &uncompanied),
        ("records with and without a company", &mixed),
        ("no record", &none),
        ("model-level", &model_level),
    ] {
        assert!(call.widened(), "{name}");
        // active in B only, A's revision unreadable to the clerk: refused, never Unmanaged
        let error = f.resolve(clerk_ab, &[b], call).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{name}: {error}");
        assert_eq!(error.message(), UNAPPROVABLE, "{name}");
        // the administrator of both, active in A where it reads A's revision, sees why
        let error = f.resolve(admin_ab, &[a], call).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{name}: {error}");
        assert_eq!(error.message(), a_conflict, "{name}");
        // the administrator active in B only cannot read A's revision either
        let error = f.resolve(admin_ab, &[b], call).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{name}: {error}");
        assert_eq!(error.message(), UNAPPROVABLE, "{name}");
    }
    // Not widened: a call on records of B only resolves the active companies plus B.
    let on_b = partner_write(b);
    assert!(!on_b.widened());
    assert_eq!(
        f.resolve(clerk_ab, &[b], &on_b).await.unwrap(),
        PolicyOutcome::Unmanaged
    );
    assert_eq!(on_b.target_companies(), &[company_id(b)]);
    // a user allowed in A only meets A's revision as before
    assert_eq!(
        f.resolve(admin_a, &[], &create).await.unwrap(),
        PolicyOutcome::Deny {
            revision: revision(a_deny)
        }
    );

    // A known company of the new records outside the caller's allowed companies is resolved too.
    f.archive(admin_a, a_deny).await;
    let c_deny = f
        .create_unfenced(
            admin_c,
            select(
                policy("c deny", "write", "deny"),
                Some(c),
                Some("res.partner"),
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        f.resolve(clerk_ab, &[b], &create).await.unwrap(),
        PolicyOutcome::Unmanaged,
        "A and B have no active revision now"
    );
    let into_c = ApprovalCall::create("res.partner", "create", &[company_id(c)]).unwrap();
    assert_eq!(into_c.target_companies(), &[company_id(c)]);
    let error = f.resolve(clerk_ab, &[b], &into_c).await.unwrap_err();
    validation(&error, UNAPPROVABLE);
    // C's administrator, allowed in C only, meets C's revision for the same call
    assert_eq!(
        f.resolve(admin_c, &[], &into_c).await.unwrap(),
        PolicyOutcome::Deny {
            revision: revision(c_deny)
        }
    );
    f.db.cleanup().await.unwrap();
}

/// the requester is allowed in: an approver allowed only in the requester's active company is not
/// eligible for a create)
#[test]
fn a_widened_approval_needs_an_approver_of_every_allowed_company() {
    run(a_widened_approval_needs_an_approver_of_every_allowed_company_body);
}

async fn a_widened_approval_needs_an_approver_of_every_allowed_company_body() {
    let mut f = fixture().await;
    let b = f.main;
    let a = f.company("Company A").await;
    let admin = f
        .user("policy-admin", &[b, a], &[f.group_user, f.group_system])
        .await;
    let requester = f.user("requester", &[b, a], &[f.group_user]).await;
    let approver_a = f.user("approver-a", &[a], &[f.group_user]).await;
    let approver_ab = f.user("approver-ab", &[b, a], &[f.group_user]).await;
    f.refresh().await;
    let id = f
        .create_unfenced(
            admin,
            approvers(
                select(
                    policy("partners", "write", "approve"),
                    None,
                    Some("res.partner"),
                    None,
                ),
                &[approver_a, approver_ab],
                &[],
            ),
        )
        .await
        .unwrap();
    // a create by the requester active in A: approver-a is not allowed in B
    let create = ApprovalCall::create("res.partner", "create", &[]).unwrap();
    let (got, eligible) = approved_by(f.resolve(requester, &[a], &create).await.unwrap());
    assert_eq!(got, revision(id));
    assert_eq!(eligible, user_ids(&[approver_ab]));
    // not widened: a write on a partner of A, active in A, keeps approver-a
    let outcome = f.resolve(requester, &[a], &partner_write(a)).await.unwrap();
    let (_, eligible) = approved_by(outcome);
    assert_eq!(eligible, user_ids(&[approver_a, approver_ab]));
    // with approver-ab archived, nobody is allowed in every company of the create: refused
    f.sql(
        "UPDATE res_users SET active = false WHERE id = $1",
        &[&approver_ab],
    )
    .await;
    let error = f.resolve(requester, &[a], &create).await.unwrap_err();
    validation(&error, UNAPPROVABLE);
    let error = f.resolve(admin, &[a], &create).await.unwrap_err();
    validation(
        &error,
        &refused(
            id,
            "partners",
            "no eligible approver other than the requester is configured for this call (ADR \
             0169 D2).",
        ),
    );
    f.db.cleanup().await.unwrap();
}

/// `true` when no active revision named `fenced` exists.
async fn no_active_fenced_revision(f: &Fixture) -> bool {
    let rows =
        f.db.client()
            .await
            .unwrap()
            .query(
                "SELECT id::int8 FROM ir_approval_policy WHERE name = 'fenced' AND active",
                &[],
            )
            .await
            .unwrap();
    rows.is_empty()
}

const FENCED_TAIL: &str = "is not available yet in this release. Save the revision archived to \
                           prepare it.";

#[test]
fn an_active_user_approve_or_deny_revision_is_refused_until_enforcement_exists() {
    run(an_active_user_approve_or_deny_revision_is_refused_until_enforcement_exists_body);
}

async fn an_active_user_approve_or_deny_revision_is_refused_until_enforcement_exists_body() {
    let f = fixture().await;
    let admin = f
        .user("fence-admin", &[f.main], &[f.group_user, f.group_system])
        .await;
    let approver = f.user("fence-approver", &[f.main], &[f.group_user]).await;
    let fenced = |name: &str, id: i64| {
        format!(
            "Approval policy revision '{name}' (id {id}): requiring approval for or denying calls \
             of users {FENCED_TAIL}"
        )
    };
    let wanted = |outcome: &str| {
        let vals = select(
            policy("fenced", "write", outcome),
            None,
            Some("res.partner"),
            None,
        );
        if outcome == "approve" {
            approvers(vals, &[approver], &[])
        } else {
            vals
        }
    };

    for outcome in ["approve", "deny"] {
        // create active: refused (the error names the row it inserted, then the transaction rolls back)
        let error = f.create(admin, wanted(outcome)).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{outcome}: {error}");
        assert!(error.message().ends_with(FENCED_TAIL), "{error}");
        assert!(no_active_fenced_revision(&f).await);

        // archived: created, stays archived; activating it (write or action_unarchive) is refused
        let id = f
            .create(admin, wanted(outcome).set_cell("active", Cell::Bool(false)))
            .await
            .unwrap();
        let error = f
            .write(
                admin,
                id,
                RawVals::new().set_cell("active", Cell::Bool(true)),
            )
            .await
            .unwrap_err();
        validation(&error, &fenced("fenced", id));
        let mut tx = f.begin(admin).await;
        let archived = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
        let error = archived.action_unarchive(&mut tx).await.unwrap_err();
        tx.rollback().await.unwrap();
        assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
        assert!(error.message().ends_with(FENCED_TAIL), "{error}");
        assert!(no_active_fenced_revision(&f).await);
        // revisions are never deleted through the model; SQL clears the row for the next round
        f.sql("DELETE FROM ir_approval_policy WHERE id = $1", &[&id])
            .await;
    }

    // the superuser is not exempt
    let mut tx = f.orm.begin(SUPERUSER_ID).await.unwrap();
    let error = tx
        .env()
        .records(MODEL)
        .unwrap()
        .create(&mut tx, vec![wanted("deny")])
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    assert!(error.message().ends_with(FENCED_TAIL), "{error}");

    // a data file is not exempt: the loader reaches the model's create
    let mut tx = f.orm.begin(SUPERUSER_ID).await.unwrap();
    let mut session = erp_data::load::LoadSession::new(erp_data::load::SessionOptions::new(
        "20.0",
        Arc::new(erp_data::load::NoFiles),
    ));
    session.begin_operation(["base"]).unwrap();
    let operations = erp_data::parse_xml_str(
        "<erp><record id=\"fenced_by_file\" model=\"ir.approval.policy\">\
         <field name=\"name\">fenced</field><field name=\"principal_kind\">user</field>\
         <field name=\"effect\">write</field><field name=\"outcome\">deny</field></record></erp>",
        "fenced.xml",
        "base",
        false,
    )
    .unwrap();
    let mut operations = operations.into_iter().map(Ok);
    let file =
        erp_data::load::FileContext::new("base", "fenced.xml", erp_data::ConvertMode::Update);
    let error = Box::pin(session.load_file(&mut tx, &file, &mut operations))
        .await
        .unwrap_err();
    assert_eq!(error.orm_kind(), Some(ErrorKind::Validation), "{error}");
    tx.rollback().await.unwrap();
    assert!(no_active_fenced_revision(&f).await);

    // a database that already holds an active one (made by SQL): a copy is never active, archiving
    // is allowed, and every other combination is created
    let old = f.create_unfenced(admin, wanted("approve")).await.unwrap();
    let mut tx = f.begin(admin).await;
    let source = tx.env().records(MODEL).unwrap().browse([old]).unwrap();
    let error = source.copy(&mut tx, None).await.unwrap_err();
    tx.rollback().await.unwrap();
    // the tie with the original is refused before the fence is reached: one outcome, a refusal
    assert_eq!(error.kind(), ErrorKind::Validation, "{error}");
    f.archive(admin, old).await;
    assert!(no_active_fenced_revision(&f).await);
    f.create(admin, policy("fenced", "write", "allow"))
        .await
        .unwrap();
    f.create(
        admin,
        policy("fenced agent", "write", "deny")
            .set_cell("principal_kind", Cell::Str("agent".into())),
    )
    .await
    .unwrap();
    f.db.cleanup().await.unwrap();
}
