// SPDX-License-Identifier: MIT OR Apache-2.0
//! The `with_user` engine rule (A4, slice P5,
//! principal is `System`, is the target already, or holds `principal.impersonate`; a switch to the
//! superuser id from any other principal is refused; the caller entry opens a user principal only.
//!
//! Every case runs under the real compiled policy (`erp_security::source::PolicySource` over the
//! fixture's rows, the principal resolver) with every `ir.capability` row `addons/base` ships
//! (`data/ir.capability.csv`, `principal.impersonate` mapped to `base.group_system`). The users:
//! `a` (internal, no capability), `hold` (internal and administrator: holds the capability) and
//! `t` (internal, the usual target).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod company_rows_support;
mod users_support;

use std::path::PathBuf;
use std::sync::Arc;

use company_rows_support::UserKind::Internal;
use company_rows_support::{PARTNER_AND_USERS, fixture_company, fixture_user, insert_generated};
use erp_data::{ConvertMode, CsvImportResult, parse_csv};
use erp_orm::audit::ElevationReason;
use erp_orm::jobs::EnqueueRequest;
use erp_orm::{
    BeginOptions, Context, ElevationScope, Env, Operation, OperationSet, Orm, PolicyProvider,
    PrincipalHint, PrincipalKind, ReadAuditOwner, SUPERUSER_ID, ScopeItem, UserId,
};
use erp_sdk::{Domain, Error, ErrorKind, Records, Transaction, Value};
use erp_security::principal::PRINCIPAL_IMPERSONATE;
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};
use users_support::{harness, map, s};

const PARTNER: &str = "res.partner";
const REASON: &str = "prin_switch.test.block";

struct World {
    h: Harness,
    orm: Orm,
    client: Client,
    a: i64,
    hold: i64,
    t: i64,
    partner: i64,
}

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn group(client: &Client, name: &str, xmlid: &str) -> i64 {
    let id = one(
        client,
        "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
        &[&name],
    )
    .await;
    let (module, local) = xmlid.split_once('.').unwrap();
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ($1, $2, 'res.groups', $3)",
            &[&module, &local, &id],
        )
        .await
        .unwrap();
    id
}

async fn access(client: &Client, name: &str, model: i64, group: i64, ops: &str) {
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, active) \
             VALUES ($1, $2, $3, $4, true)",
            &[&name, &model, &group, &ops],
        )
        .await
        .unwrap();
}

/// Every row of `addons/base/data/ir.capability.csv`, as base installs them.
async fn base_capabilities(client: &Client) {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../addons/base/data/ir.capability.csv");
    let content = std::fs::read_to_string(path).unwrap();
    let CsvImportResult::Loaded(import) =
        parse_csv(&content, "ir.capability.csv", "base", ConvertMode::Init).unwrap()
    else {
        panic!("the capability data loads");
    };
    assert_eq!(import.fields(), ["id", "name", "groups", "description"]);
    let mut impersonate = false;
    for row in import.rows() {
        let [_, name, groups, description] = row.values() else {
            panic!("four cells on line {}", row.line());
        };
        impersonate |= name == "principal.impersonate";
        client
            .execute(
                "INSERT INTO ir_capability (name, groups, description) VALUES ($1, $2, $3)",
                &[name, groups, description],
            )
            .await
            .unwrap();
    }
    assert!(impersonate, "base maps principal.impersonate");
}

/// The compiled policy over the fixture's rows and the principal resolver; `impersonate` names
/// the capability that authorizes a switch to another user.
async fn compiled_orm(h: &Harness, impersonate: bool) -> Orm {
    let pool = || erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    let source = PolicySource::build(Arc::clone(h.registry()), pool())
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let mut builder = Orm::builder(Arc::clone(h.registry()), pool())
        .policy(provider)
        .resolver(Resolver::id_order());
    if impersonate {
        builder = builder.impersonate_capability(&PRINCIPAL_IMPERSONATE);
    }
    let orm = builder.build().unwrap();
    orm.check_audit_store().await.unwrap();
    orm
}

async fn world_with(impersonate: bool) -> World {
    let h = harness().await;
    let client = connect(&h).await;
    let internal = group(&client, "Internal User", "base.group_user").await;
    let system = group(&client, "Administrator", "base.group_system").await;
    let company = fixture_company(&client).await;
    let partners = one(
        &client,
        "INSERT INTO ir_model (name, model, state, transient) \
         VALUES ('Contact', 'res.partner', 'base', false) RETURNING id::int8",
        &[],
    )
    .await;
    let users = one(
        &client,
        "INSERT INTO ir_model (name, model, state, transient) \
         VALUES ('User', 'res.users', 'base', false) RETURNING id::int8",
        &[],
    )
    .await;
    // installation emits the generated company restrictions; this database has no installation
    insert_generated(&client, h.registry(), &PARTNER_AND_USERS).await;
    access(&client, "internal read contacts", partners, internal, "r").await;
    access(&client, "admins change contacts", partners, system, "cru").await;
    access(&client, "internal read users", users, internal, "r").await;
    base_capabilities(&client).await;
    let a = fixture_user(&client, "a", &[internal], Internal, company).await;
    let hold = fixture_user(&client, "hold", &[internal, system], Internal, company).await;
    let t = fixture_user(&client, "t", &[internal], Internal, company).await;
    let partner = one(
        &client,
        "INSERT INTO res_partner (name, active) VALUES ('Shared', true) RETURNING id::int8",
        &[],
    )
    .await;
    let orm = compiled_orm(&h, impersonate).await;
    World {
        h,
        orm,
        client,
        a,
        hold,
        t,
        partner,
    }
}

async fn world() -> World {
    world_with(true).await
}

fn uid(n: i64) -> UserId {
    UserId::new(n).unwrap()
}

async fn begin(orm: &Orm, n: i64) -> Transaction {
    orm.begin(uid(n)).await.unwrap()
}

async fn begin_system(orm: &Orm) -> Transaction {
    orm.begin(SUPERUSER_ID).await.unwrap()
}

fn partner(env: &Env, id: i64) -> Records {
    env.records(PARTNER).unwrap().browse([id]).unwrap()
}

fn impersonation_refusal(switcher: i64, target: i64) -> String {
    format!(
        "user {switcher} may not act as user {target}: with_user needs the capability \
         'principal.impersonate'"
    )
}

fn superuser_refusal(switcher: i64) -> String {
    format!(
        "only the system principal may act as the superuser (user {switcher} called with_user(1))"
    )
}

#[track_caller]
fn assert_error(error: &Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind, "{error}");
    assert_eq!(error.message(), message);
}

fn read_scope(tx: &Transaction) -> ElevationScope {
    let model = tx.registry().model_id(PARTNER).unwrap();
    ElevationScope::new([ScopeItem::new(model, OperationSet::of([Operation::Read])).unwrap()])
        .unwrap()
}

/// Every engine entry the case names refuses `env` with `kind` and `message`: a record read, a
/// search, a write, a model check, a scoped block, a job enqueue, a capability question, the
/// companies, and the audited unscoped escape hatch.
async fn refused_everywhere(
    tx: &mut Transaction,
    env: &Env,
    id: i64,
    kind: ErrorKind,
    message: &str,
) {
    let record = partner(env, id);
    let model = env.records(PARTNER).unwrap();
    assert_error(&record.value(tx, "name").await.unwrap_err(), kind, message);
    assert_error(
        &model.search(tx, &Domain::TRUE).await.unwrap_err(),
        kind,
        message,
    );
    assert_error(
        &record
            .write_values(tx, &map(&[("name", s("changed"))]))
            .await
            .unwrap_err(),
        kind,
        message,
    );
    assert_error(
        &model.check_access(tx, Operation::Read).await.unwrap_err(),
        kind,
        message,
    );
    let scope = read_scope(tx);
    let reason = ElevationReason::new(REASON).unwrap();
    assert_error(
        &tx.elevated(env, reason, scope, async |_, _| Ok(()))
            .await
            .unwrap_err(),
        kind,
        message,
    );
    assert_error(
        &tx.enqueue_many(
            env,
            vec![EnqueueRequest::new("prin_switch.never_registered")],
        )
        .await
        .unwrap_err(),
        kind,
        message,
    );
    assert_error(
        &tx.has_capability(env, &PRINCIPAL_IMPERSONATE)
            .await
            .unwrap_err(),
        kind,
        message,
    );
    assert_error(&tx.companies(env).await.unwrap_err(), kind, message);
    assert_error(
        &tx.audited_unscoped_sudo(env, "prin_switch").unwrap_err(),
        kind,
        message,
    );
}

/// The name of the shared contact, read by SQL (no environment involved).
async fn stored_name(w: &World) -> String {
    w.client
        .query_one("SELECT name FROM res_partner WHERE id = $1", &[&w.partner])
        .await
        .unwrap()
        .get(0)
}

/// itself, also from superuser mode)
#[tokio::test]
async fn system_and_the_same_subject_are_admitted() {
    let w = world().await;
    let mut tx = begin_system(&w.orm).await;
    let system = tx.env();
    let as_a = system.with_user(w.a);
    assert_eq!(as_a.uid(), w.a);
    assert_eq!(
        partner(&as_a, w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap(),
        s("Shared")
    );
    // the superuser id from System is System again
    let again = system.with_user(SUPERUSER_ID);
    assert_eq!(again, system);
    assert_eq!(
        partner(&again, w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap(),
        s("Shared")
    );
    drop(tx);

    let mut tx = begin(&w.orm, w.a).await;
    let own = tx.env();
    for same in [own.with_user(w.a), own.sudo(true).with_user(w.a)] {
        assert_eq!(same.uid(), w.a);
        assert!(!same.su());
        assert_eq!(
            partner(&same, w.partner)
                .value(&mut tx, "name")
                .await
                .unwrap(),
            s("Shared")
        );
    }
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// admitted and acts with the target's rights)
#[tokio::test]
async fn a_user_needs_the_capability_to_act_as_another() {
    let w = world().await;
    // what `a` itself is refused, read from `a`'s own transaction
    let mut tx = begin(&w.orm, w.a).await;
    let own_refusal = partner(&tx.env(), w.partner)
        .write_values(&mut tx, &map(&[("name", s("by a"))]))
        .await
        .unwrap_err();
    assert_eq!(own_refusal.kind(), ErrorKind::Access);
    drop(tx);

    // a to hold: refused at the first use
    let mut tx = begin(&w.orm, w.a).await;
    let up = tx.env().with_user(w.hold);
    assert_eq!(up.uid(), w.hold, "the environment names the target");
    refused_everywhere(
        &mut tx,
        &up,
        w.partner,
        ErrorKind::Access,
        &impersonation_refusal(w.a, w.hold),
    )
    .await;
    drop(tx);

    // hold to a: admitted, with a's rights (reads, cannot write)
    let mut tx = begin(&w.orm, w.hold).await;
    let down = tx.env().with_user(w.a);
    assert_eq!(
        partner(&down, w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap(),
        s("Shared")
    );
    let refusal = partner(&down, w.partner)
        .write_values(&mut tx, &map(&[("name", s("as a"))]))
        .await
        .unwrap_err();
    assert_error(&refusal, own_refusal.kind(), own_refusal.message());
    assert!(
        tx.has_capability(&tx.env(), &PRINCIPAL_IMPERSONATE)
            .await
            .unwrap()
    );
    assert!(
        !tx.has_capability(&down, &PRINCIPAL_IMPERSONATE)
            .await
            .unwrap()
    );
    // hold itself may write
    partner(&tx.env(), w.partner)
        .write_values(&mut tx, &map(&[("name", s("by hold"))]))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    w.h.cleanup().await.unwrap();
}

/// superuser mode and on a recordset; the refused environment is never superuser, `System` or
/// subject 1)
#[tokio::test]
async fn a_user_is_never_admitted_as_the_superuser() {
    let w = world().await;
    let mut tx = begin(&w.orm, w.a).await;
    let base = tx.env();
    let message = superuser_refusal(w.a);
    let made = [
        base.with_user(SUPERUSER_ID),
        base.with_user(1),
        base.sudo(true).with_user(SUPERUSER_ID),
        base.with_user(SUPERUSER_ID).sudo(true),
        partner(&base, w.partner)
            .with_user(SUPERUSER_ID)
            .env()
            .clone(),
        partner(&base.sudo(true), w.partner)
            .with_user(SUPERUSER_ID)
            .env()
            .clone(),
    ];
    for env in &made {
        assert!(!env.su(), "{env:?}");
        assert!(!env.is_superuser());
        assert_eq!(env.principal().kind(), PrincipalKind::User);
        assert_eq!(env.uid(), w.a);
        refused_everywhere(&mut tx, env, w.partner, ErrorKind::Access, &message).await;
    }
    // further derivations keep the refusal
    for env in [
        made[0].sudo(false),
        made[0].with_context([("lang", Value::str("en_US"))]),
        made[0].with_debug(true),
        made[0].with_user(w.a),
    ] {
        assert!(!env.su());
        assert_error(
            &partner(&env, w.partner)
                .value(&mut tx, "name")
                .await
                .unwrap_err(),
            ErrorKind::Access,
            &message,
        );
    }
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// another user or the superuser id, through every entry kind)
#[tokio::test]
async fn the_immediate_switcher_decides_in_a_chain() {
    let w = world().await;
    let mut tx = begin_system(&w.orm).await;
    let as_a = tx.env().with_user(w.a);
    refused_everywhere(
        &mut tx,
        &as_a.with_user(w.t),
        w.partner,
        ErrorKind::Access,
        &impersonation_refusal(w.a, w.t),
    )
    .await;
    let back = as_a.with_user(SUPERUSER_ID);
    assert!(!back.su());
    assert_eq!(back.uid(), w.a);
    refused_everywhere(
        &mut tx,
        &back,
        w.partner,
        ErrorKind::Access,
        &superuser_refusal(w.a),
    )
    .await;
    // a holder in the middle of a chain is admitted: System, then hold, then t
    let through_holder = tx.env().with_user(w.hold).with_user(w.t);
    assert_eq!(
        partner(&through_holder, w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap(),
        s("Shared")
    );
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// refusal comes first and is unchanged)
#[tokio::test]
async fn a_scope_never_carries_the_capability() {
    let w = world().await;
    let mut tx = begin(&w.orm, w.a).await;
    let base = tx.env();
    let scope = read_scope(&tx);
    let reason = ElevationReason::new(REASON).unwrap();
    let (switched, widened) = tx
        .elevated(&base, reason, scope, async |tx, env| {
            let switched = env
                .with_user(w.hold)
                .records(PARTNER)?
                .search(tx, &Domain::TRUE)
                .await
                .unwrap_err();
            let widened = env.with_user(SUPERUSER_ID).companies().unwrap_err();
            Ok((switched, widened))
        })
        .await
        .unwrap();
    assert_error(
        &switched,
        ErrorKind::Access,
        &impersonation_refusal(w.a, w.hold),
    );
    assert_eq!(widened.kind(), ErrorKind::Programming);
    assert!(
        widened
            .message()
            .starts_with("an environment of elevation block ")
            && widened.message().ends_with(&format!(
                "(reason '{REASON}') cannot be widened to superuser mode"
            )),
        "{widened}"
    );
    drop(tx);
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_refused_switch_writes_nothing_and_records_no_elevation() {
    let w = world().await;
    let count = async || -> i64 {
        w.client
            .query_one("SELECT count(*) FROM erp_event.ir_audit_entry", &[])
            .await
            .unwrap()
            .get(0)
    };
    let before = count().await;
    let mut tx = w.orm.begin_opts(BeginOptions::new(uid(w.a))).await.unwrap();
    for (env, message) in [
        (tx.env().with_user(SUPERUSER_ID), superuser_refusal(w.a)),
        (
            tx.env().sudo(true).with_user(SUPERUSER_ID),
            superuser_refusal(w.a),
        ),
        (
            tx.env().with_user(w.hold),
            impersonation_refusal(w.a, w.hold),
        ),
    ] {
        let record = partner(&env, w.partner);
        assert_error(
            &record
                .write_values(&mut tx, &map(&[("name", s("refused"))]))
                .await
                .unwrap_err(),
            ErrorKind::Access,
            &message,
        );
        assert_error(
            &tx.audited_unscoped_sudo(&env, "prin_switch").unwrap_err(),
            ErrorKind::Access,
            &message,
        );
    }
    tx.commit().await.unwrap();
    assert_eq!(stored_name(&w).await, "Shared");
    assert_eq!(count().await, before, "no audit evidence was written");
    w.h.cleanup().await.unwrap();
}

/// unscoped escape hatch refuses a switched environment before anything is recorded: outside a
/// block with the switch refusal, inside a block with the widening refusal, which comes first;
/// the owner's evidence then holds only the block's own observation, and the admitted call on
/// the base environment is recorded, review cycle 10)
#[tokio::test]
async fn an_owned_read_refuses_a_switched_unscoped_sudo_before_recording() {
    let w = world().await;
    let mut tx = w
        .orm
        .begin_opts(BeginOptions::new(uid(w.a)).read_only_audited(
            "res.partner.read",
            ReadAuditOwner::new("prin-switch").unwrap(),
        ))
        .await
        .unwrap();
    assert!(tx.intent().is_read());
    let base = tx.env();
    // the switcher is resolved by a first read, so the switch is decided, not unresolved
    assert_eq!(
        partner(&base, w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap(),
        Value::str("Shared")
    );
    for (env, message) in [
        (base.with_user(w.hold), impersonation_refusal(w.a, w.hold)),
        (base.with_user(SUPERUSER_ID), superuser_refusal(w.a)),
    ] {
        assert_error(
            &tx.audited_unscoped_sudo(&env, "prin_switch").unwrap_err(),
            ErrorKind::Access,
            &message,
        );
    }
    let evidence = tx.take_audit_attempt();
    assert!(!evidence.has_elevation(), "nothing recorded: {evidence:?}");
    let scope = read_scope(&tx);
    let reason = ElevationReason::new(REASON).unwrap();
    let widened = tx
        .elevated(&base, reason, scope, async |tx, env| {
            Ok(tx
                .audited_unscoped_sudo(&env.with_user(w.hold), "prin_switch")
                .unwrap_err())
        })
        .await
        .unwrap();
    assert_eq!(widened.kind(), ErrorKind::Programming, "{widened}");
    assert!(
        widened.message().ends_with(&format!(
            "(reason '{REASON}') cannot be widened to superuser mode"
        )),
        "{widened}"
    );
    let shown = format!("{:?}", tx.take_audit_attempt());
    assert!(
        shown.contains("Scoped"),
        "the block's own observation: {shown}"
    );
    assert!(!shown.contains("core.sudo.unscoped"), "{shown}");
    // the same owned transaction records the admitted call on the base environment
    tx.audited_unscoped_sudo(&base, "prin_switch").unwrap();
    let shown = format!("{:?}", tx.take_audit_attempt());
    assert!(shown.contains("core.sudo.unscoped"), "{shown}");
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// user, without a switch)
#[tokio::test]
async fn has_group_about_another_user_still_answers() {
    let w = world().await;
    let mut tx = begin(&w.orm, w.a).await;
    let env = tx.env();
    let ask = async |tx: &mut Transaction, target: i64, group: &str| {
        env.records("res.users")
            .unwrap()
            .browse([target])
            .unwrap()
            .call::<(String,), bool>(tx, "has_group", (group.to_owned(),))
            .await
    };
    assert!(ask(&mut tx, w.hold, "base.group_system").await.unwrap());
    assert!(!ask(&mut tx, w.t, "base.group_system").await.unwrap());
    assert!(ask(&mut tx, w.t, "base.group_user").await.unwrap());
    // the core entry answers the same without an environment of the other user
    assert!(
        tx.user_has_group(&env, uid(w.hold), "base.group_system")
            .await
            .unwrap()
    );
    assert!(
        !tx.user_has_group(&env, uid(w.t), "base.group_system")
            .await
            .unwrap()
    );
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// The caller entry opens a transaction of kind `user` for an ordinary user id; the superuser id
/// is refused before any transaction exists (also by the hint, the options and the environment
/// entry); uid 0 is refused by the id validation.
#[tokio::test]
async fn the_caller_entry_opens_a_user_principal_only() {
    let w = world().await;
    let mut tx = w.orm.begin_caller(uid(w.a)).await.unwrap();
    let env = tx.env();
    assert_eq!(env.principal().kind(), PrincipalKind::User);
    assert_eq!(env.uid(), w.a);
    assert!(!env.su());
    assert_eq!(
        partner(&env, w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap(),
        s("Shared")
    );
    drop(tx);
    assert_eq!(
        PrincipalHint::caller(uid(w.a)).unwrap().kind(),
        PrincipalKind::User
    );
    let caller_env = w.orm.env_caller(uid(w.a), Context::new()).unwrap();
    assert_eq!(caller_env.principal().kind(), PrincipalKind::User);

    let message = "the superuser cannot be opened as a caller: core code opens the system \
                   principal itself";
    assert_error(
        &w.orm.begin_caller(SUPERUSER_ID).await.err().unwrap(),
        ErrorKind::Access,
        message,
    );
    let hint = PrincipalHint::caller(SUPERUSER_ID).unwrap_err();
    assert_error(&Error::from(hint), ErrorKind::Access, message);
    assert_error(
        &Error::from(BeginOptions::caller(SUPERUSER_ID).unwrap_err()),
        ErrorKind::Access,
        message,
    );
    assert_error(
        &w.orm.env_caller(SUPERUSER_ID, Context::new()).unwrap_err(),
        ErrorKind::Access,
        message,
    );
    let zero = Error::from(UserId::new(0).unwrap_err());
    assert_error(
        &zero,
        ErrorKind::Programming,
        "invalid user id 0: ids are non-zero",
    );
    w.h.cleanup().await.unwrap();
}

/// nothing an admitted environment of the same subject cached)
#[tokio::test]
async fn a_pending_switch_is_refused_before_the_cache() {
    let w = world().await;
    let mut tx = begin(&w.orm, w.a).await;
    let admitted = w.orm.env(SUPERUSER_ID, Context::new()).with_user(w.t);
    assert_eq!(
        partner(&admitted, w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap(),
        s("Shared")
    );
    let pending = tx.env().with_user(w.t);
    // the same subject and kind (binding keys carry no actor); only the switch differs
    assert_eq!(
        (admitted.uid(), admitted.principal().kind()),
        (pending.uid(), pending.principal().kind())
    );
    assert_ne!(admitted, pending);
    let message = impersonation_refusal(w.a, w.t);
    assert_error(
        &partner(&pending, w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap_err(),
        ErrorKind::Access,
        &message,
    );
    assert_error(
        &partner(&pending, w.partner)
            .read(&mut tx, &["name"])
            .await
            .unwrap_err(),
        ErrorKind::Access,
        &message,
    );
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// An ORM built without the capability that authorizes impersonation refuses every switch to
/// another user by a non-System principal, naming the missing builder call; `System` switches
/// stay admitted.
#[tokio::test]
async fn without_the_capability_configured_a_switch_to_another_user_is_refused() {
    let w = world_with(false).await;
    let mut tx = begin(&w.orm, w.hold).await;
    let error = partner(&tx.env().with_user(w.a), w.partner)
        .value(&mut tx, "name")
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &format!(
            "user {} may not act as user {}: with_user from a principal that is not the system \
             needs the capability that authorizes impersonation, and this ORM was built without \
             one (OrmBuilder::impersonate_capability)",
            w.hold, w.a
        ),
    );
    drop(tx);
    let mut tx = begin_system(&w.orm).await;
    let as_a = tx.env().with_user(w.a);
    assert_eq!(
        partner(&as_a, w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap(),
        s("Shared")
    );
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// in the transaction, is admitted for a holder in the chain and refused with the case's `Access`
/// error for a user without the capability)
#[tokio::test]
async fn a_method_call_decides_the_switch_before_anything_else() {
    let w = world().await;
    let ask = async |tx: &mut Transaction, env: &Env, target: i64| {
        env.records("res.users")
            .unwrap()
            .browse([target])
            .unwrap()
            .call::<(String,), bool>(tx, "has_group", ("base.group_user".to_owned(),))
            .await
    };
    // System, then hold, then t: admitted (t asks about itself)
    let mut tx = begin_system(&w.orm).await;
    let through_holder = tx.env().with_user(w.hold).with_user(w.t);
    assert!(ask(&mut tx, &through_holder, w.t).await.unwrap());
    drop(tx);
    // System, then a, then t: refused with the case's text, the first thing the transaction does
    let mut tx = begin_system(&w.orm).await;
    let through_a = tx.env().with_user(w.a).with_user(w.t);
    assert_error(
        &ask(&mut tx, &through_a, w.t).await.unwrap_err(),
        ErrorKind::Access,
        &impersonation_refusal(w.a, w.t),
    );
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// A4 (slice P5): the principal of an environment switched by a non-System root cannot
/// open a transaction of its own, which would skip the switch decision; a chain rooted at `System`
/// opens one (the principal records the root only; P6's lint covers that entry)
#[tokio::test]
async fn a_switched_principal_cannot_open_a_transaction() {
    let w = world().await;
    let tx = begin(&w.orm, w.a).await;
    let switched = tx.env().with_user(w.t).principal().clone();
    drop(tx);
    let error = w
        .orm
        .begin_opts(BeginOptions::for_principal(switched))
        .await
        .err()
        .unwrap();
    assert_error(
        &error,
        ErrorKind::Access,
        &format!(
            "a transaction cannot be opened for user {} as switched to by user {}: an \
             environment made by with_user is admitted by the engine only; open the \
             transaction for the caller (Orm::begin_caller)",
            w.t, w.a
        ),
    );
    let by_system = w
        .orm
        .env(SUPERUSER_ID, Context::new())
        .with_user(w.t)
        .principal()
        .clone();
    let mut tx = w
        .orm
        .begin_opts(BeginOptions::for_principal(by_system))
        .await
        .unwrap();
    assert_eq!(tx.env().uid(), w.t);
    assert_eq!(
        partner(&tx.env(), w.partner)
            .value(&mut tx, "name")
            .await
            .unwrap(),
        s("Shared")
    );
    drop(tx);
    w.h.cleanup().await.unwrap();
}
