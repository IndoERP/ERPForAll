//! The company scope of base's contacts and users (
//!
//! Two kinds of database:
//!
//! - `users_support::harness()` (base and `ir.model.data`, no access policy): the per-record
//!   compute of `res.partner.partner_share` and the set-based recompute behind group changes. The
//!   expected value of every partner is also stated as a statement over the stored user rows
//!   ([`rule`]), so the two paths are compared with an independent reading of the rule.
//! - [`scoped`]: the generated restrictions of base compiled by the real policy compiler
//!   (`erp-security`), a principal with the internal-user group, allowed and active in company B
//!   only. The groups are written with SQL there (a policy-table write would rebuild the policy,
//!   which this provider does not do); users, contacts and companies go through the ORM.
//!   Expected admissions come from [`partner_admitted`] / [`user_admitted`] over the stored
//!   values, not from the generated text.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::{Arc, PoisonError, RwLock};

use erp_orm::{
    AccessPolicy, BeginOptions, BoxFuture, CompanyHint, CompanyId, GroupId, PolicyProvider,
    Principal, PrincipalBuilder, QualifiedXmlId, Resolution, Transaction, UserId,
};
use erp_sdk::{Command, Domain, ErrorKind, RawVals, RecId, Value};
use erp_security::company_access::generate;
use erp_security::rows::{GroupRow, Implication, PolicyRows, RuleId, RuleRow};
use erp_security::snapshot::Compiler;
use erp_testkit::Harness;
use users_support::{bind, create, harness, int, map, records, s, set_cmd, user_types_and_admin};

// ------------------------------------------------------------------------------------------------
// the rule stated on stored rows

/// The rule stated on the stored user rows: a partner is not a share partner when one of
/// its users is the superuser (archived or not) or an active user with `share = false`.
async fn rule(tx: &mut Transaction, partner: i64) -> bool {
    tx.flush().await.unwrap();
    let rows = tx
        .query(
            "SELECT id, active, share FROM res_users WHERE partner_id = $1",
            &[&partner],
        )
        .await
        .unwrap();
    let superuser = erp_orm::SUPERUSER_ID.get();
    !rows.iter().any(|row| {
        let id: i64 = row.get(0);
        let active: Option<bool> = row.get(1);
        let share: bool = row.get(2);
        id == superuser || (active == Some(true) && !share)
    })
}

async fn partner(tx: &mut Transaction, name: &str) -> i64 {
    create(tx, "res.partner", &[("name", s(name))]).await
}

async fn linked_user(tx: &mut Transaction, login: &str, partner: i64, groups: &[i64]) -> i64 {
    create(
        tx,
        "res.users",
        &[
            ("name", s(login)),
            ("login", s(login)),
            ("partner_id", int(partner)),
            ("group_ids", set_cmd(groups)),
        ],
    )
    .await
}

async fn set_active(tx: &mut Transaction, user: i64, active: bool) {
    records(tx, "res.users", &[user])
        .write_values(tx, &map(&[("active", Value::Bool(active))]))
        .await
        .unwrap();
}

async fn move_user(tx: &mut Transaction, user: i64, partner: i64) {
    records(tx, "res.users", &[user])
        .write_values(tx, &map(&[("partner_id", int(partner))]))
        .await
        .unwrap();
}

async fn partner_shares(tx: &mut Transaction, ids: &[i64]) -> Vec<bool> {
    records(tx, "res.partner", ids)
        .values(tx, "partner_share")
        .await
        .unwrap()
        .into_iter()
        .map(|v| match v {
            Value::Bool(b) => b,
            other => panic!("partner_share is not a boolean: {other:?}"),
        })
        .collect()
}

/// Every partner's stored value equals [`rule`].
async fn every_partner_follows_the_rule(tx: &mut Transaction) {
    let all = tx
        .env()
        .records("res.partner")
        .unwrap()
        .with_context([("active_test", Value::Bool(false))])
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids();
    assert!(!all.is_empty());
    let values = partner_shares(tx, &all).await;
    for (id, value) in all.iter().zip(values) {
        assert_eq!(value, rule(tx, *id).await, "partner {id}");
    }
}

/// The superuser's partner (the harness inserts the superuser row archived).
async fn superuser_partner(tx: &mut Transaction) -> i64 {
    let su = partner(tx, "System").await;
    move_user(tx, erp_orm::SUPERUSER_ID.get(), su).await;
    su
}

#[tokio::test]
async fn spec_partner_046_partner_share_follows_the_active_internal_users() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = user_types_and_admin(&mut tx).await;
    let a = partner(&mut tx, "A").await;
    let b = partner(&mut tx, "B").await;
    let c = partner(&mut tx, "C").await;
    let d = partner(&mut tx, "D").await;
    let ub = linked_user(&mut tx, "ub", b, &[t.internal]).await;
    linked_user(&mut tx, "uc", c, &[t.portal]).await;
    let ud = linked_user(&mut tx, "ud", d, &[t.internal]).await;
    set_active(&mut tx, ud, false).await;
    let su = superuser_partner(&mut tx).await;
    // the superuser row is archived: the exception does not depend on `active`
    let su_active = records(&tx, "res.users", &[erp_orm::SUPERUSER_ID.get()])
        .values(&mut tx, "active")
        .await
        .unwrap();
    // NULL in the harness row: either spelling of false
    assert!(
        matches!(su_active.as_slice(), [Value::Bool(false) | Value::False]),
        "{su_active:?}"
    );

    let ids = [a, b, c, d, su];
    assert_eq!(
        partner_shares(&mut tx, &ids).await,
        [true, false, true, true, false]
    );
    for id in ids {
        let stored = partner_shares(&mut tx, &[id]).await[0];
        assert_eq!(stored, rule(&mut tx, id).await, "partner {id}");
    }

    // archiving the only internal user makes the partner a share partner, and back
    set_active(&mut tx, ub, false).await;
    assert_eq!(partner_shares(&mut tx, &[b]).await, [true]);
    set_active(&mut tx, ub, true).await;
    assert_eq!(partner_shares(&mut tx, &[b]).await, [false]);

    // moving the user: the old partner is recomputed as well as the new one
    move_user(&mut tx, ub, a).await;
    assert_eq!(partner_shares(&mut tx, &[a, b]).await, [false, true]);
    every_partner_follows_the_rule(&mut tx).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// through `share`, without a manual flush)
#[tokio::test]
async fn partner_share_follows_a_group_change_of_its_user() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = user_types_and_admin(&mut tx).await;
    let p = partner(&mut tx, "P").await;
    let u = linked_user(&mut tx, "u", p, &[t.portal]).await;
    assert_eq!(partner_shares(&mut tx, &[p]).await, [true]);
    records(&tx, "res.users", &[u])
        .write_values(&mut tx, &map(&[("group_ids", set_cmd(&[t.internal]))]))
        .await
        .unwrap();
    assert_eq!(partner_shares(&mut tx, &[p]).await, [false]);
    records(&tx, "res.users", &[u])
        .write_values(&mut tx, &map(&[("group_ids", set_cmd(&[t.portal]))]))
        .await
        .unwrap();
    assert_eq!(partner_shares(&mut tx, &[p]).await, [true]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_047_group_changes_keep_the_per_record_rule() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    users_support::installing(&mut tx, true);
    let t = user_types_and_admin(&mut tx).await;
    let d = partner(&mut tx, "D").await;
    let ud = linked_user(&mut tx, "ud", d, &[t.internal]).await;
    set_active(&mut tx, ud, false).await;
    let su = superuser_partner(&mut tx).await;
    let b = partner(&mut tx, "B").await;
    linked_user(&mut tx, "ub", b, &[t.internal]).await;
    assert_eq!(
        partner_shares(&mut tx, &[d, su, b]).await,
        [true, false, false]
    );

    // a write: a group of `ub`'s comes to imply internal (the internal set changes)
    let staff = users_support::group(&mut tx, "Staff", &[]).await;
    records(&tx, "res.groups", &[staff])
        .write_values(
            &mut tx,
            &map(&[(
                "implied_ids",
                Value::list([users_support::link_cmd(t.internal)]),
            )]),
        )
        .await
        .unwrap();
    assert_eq!(partner_shares(&mut tx, &[d, su]).await, [true, false]);
    every_partner_follows_the_rule(&mut tx).await;

    // a create: a new group implying internal
    let extra = users_support::group(&mut tx, "Extra", &[t.internal]).await;
    assert_eq!(partner_shares(&mut tx, &[d, su]).await, [true, false]);
    every_partner_follows_the_rule(&mut tx).await;

    // a delete of that group
    records(&tx, "res.groups", &[extra])
        .unlink(&mut tx)
        .await
        .unwrap();
    assert_eq!(partner_shares(&mut tx, &[d, su]).await, [true, false]);
    every_partner_follows_the_rule(&mut tx).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// their partners, and refreshes a stale value of each)
#[tokio::test]
async fn the_bulk_recompute_of_some_users_rewrites_only_their_partners() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = user_types_and_admin(&mut tx).await;
    let p1 = partner(&mut tx, "P1").await;
    let p2 = partner(&mut tx, "P2").await;
    let u1 = linked_user(&mut tx, "u1", p1, &[t.internal]).await;
    linked_user(&mut tx, "u2", p2, &[t.internal]).await;
    let want = [rule(&mut tx, p1).await, rule(&mut tx, p2).await];
    assert_eq!(want, [false, false]);
    // stale columns, as an old database or a SQL write leaves them
    tx.execute(
        "UPDATE res_partner SET partner_share = true WHERE id = ANY($1)",
        &[&vec![p1, p2]],
    )
    .await
    .unwrap();
    tx.invalidate_model("res.partner", Some(&["partner_share"]))
        .await
        .unwrap();
    erp_addon_base::group_graph::recompute_user_share(&mut tx, Some(&[u1]))
        .await
        .unwrap();
    // p1 is read back from the database (the cache was dropped by the recompute), p2 untouched
    assert_eq!(partner_shares(&mut tx, &[p1, p2]).await, [want[0], true]);
    erp_addon_base::group_graph::recompute_user_share(&mut tx, None)
        .await
        .unwrap();
    assert_eq!(partner_shares(&mut tx, &[p1, p2]).await, want);
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn user_shares(tx: &mut Transaction, ids: &[i64]) -> Vec<bool> {
    records(tx, "res.users", ids)
        .values(tx, "share")
        .await
        .unwrap()
        .iter()
        .map(bool_of)
        .collect()
}

/// and through the set-based recompute, with no group and with a group that is not internal;
/// a user in the same position is a share user)
#[tokio::test]
async fn the_superuser_is_never_a_share_user() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    user_types_and_admin(&mut tx).await;
    let staff = users_support::group(&mut tx, "Staff", &[]).await;
    let superuser = erp_orm::SUPERUSER_ID.get();
    let n = partner(&mut tx, "N").await;
    let nobody = linked_user(&mut tx, "nobody", n, &[]).await;
    // a column that says "share" for both, as the insert of the superuser row leaves it
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE res_users SET share = true WHERE id = ANY($1)",
        &[&vec![superuser, nobody]],
    )
    .await
    .unwrap();
    tx.invalidate_model("res.users", Some(&["share"]))
        .await
        .unwrap();
    erp_addon_base::group_graph::recompute_user_share(&mut tx, None)
        .await
        .unwrap();
    // without a group: the superuser is not a share user, any other user is
    assert_eq!(
        user_shares(&mut tx, &[superuser, nobody]).await,
        [false, true]
    );

    // the per-record compute, run by a group change that brings no internal group
    tx.execute(
        "UPDATE res_users SET share = true WHERE id = $1",
        &[&superuser],
    )
    .await
    .unwrap();
    tx.invalidate_model("res.users", Some(&["share"]))
        .await
        .unwrap();
    for user in [superuser, nobody] {
        records(&tx, "res.users", &[user])
            .write_values(&mut tx, &map(&[("group_ids", set_cmd(&[staff]))]))
            .await
            .unwrap();
    }
    assert_eq!(
        user_shares(&mut tx, &[superuser, nobody]).await,
        [false, true]
    );
    // and the set-based statement agrees with groups present
    erp_addon_base::group_graph::recompute_user_share(&mut tx, None)
        .await
        .unwrap();
    assert_eq!(
        user_shares(&mut tx, &[superuser, nobody]).await,
        [false, true]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ------------------------------------------------------------------------------------------------
// the generated restrictions over PostgreSQL

const GROUP: i64 = 1;

struct Swap(RwLock<Arc<dyn AccessPolicy>>);

impl PolicyProvider for Swap {
    fn current(&self) -> Arc<dyn AccessPolicy> {
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn rebuild_private<'a>(
        &'a self,
        _tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<Arc<dyn AccessPolicy>>> {
        Box::pin(async {
            Err(erp_fields::Error::programming(
                "this test writes no policy table through the ORM",
            ))
        })
    }
}

fn builder() -> erp_testkit::HarnessBuilder {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
}

fn policy_rows(registry: &erp_registry::Registry) -> PolicyRows {
    let gid = |n| GroupId::new(n).unwrap();
    let xml = |t: &str| QualifiedXmlId::parse(t).unwrap();
    let group = |n, name: &str, x: &str| GroupRow::new(gid(n), name).unwrap().with_xmlid(xml(x));
    let mut builder = PolicyRows::builder(1)
        .group(group(GROUP, "Internal User", "base.group_user"))
        .group(group(2, "Everyone", "base.group_everyone"))
        .group(group(3, "Portal", "base.group_portal").with_share(true))
        .group(group(4, "Technical Features", "base.group_no_one"))
        .group(group(5, "Administrator", "base.group_system"))
        .group(group(6, "Public", "base.group_public").with_share(true))
        .implication(Implication::new(gid(GROUP), gid(2)))
        .implication(Implication::new(gid(3), gid(2)))
        .implication(Implication::new(gid(5), gid(GROUP)));
    let mut id = 1;
    for g in generate(registry).unwrap() {
        builder = builder
            .rule(
                RuleRow::new(RuleId::new(id).unwrap(), "perm", g.model_name(), "crud")
                    .unwrap()
                    .with_group(gid(GROUP)),
            )
            .rule(
                RuleRow::new(
                    RuleId::new(id + 1).unwrap(),
                    g.name(),
                    g.model_name(),
                    "crud",
                )
                .unwrap()
                .with_domain(g.text())
                .with_generated(),
            );
        id += 2;
    }
    // companies are read for names and hierarchies; nothing restricts them here
    builder = builder.rule(
        RuleRow::new(RuleId::new(id).unwrap(), "perm company", "res.company", "r")
            .unwrap()
            .with_group(gid(GROUP)),
    );
    // administrators manage groups, as `access_groups_system` of base's `ir.access.csv`
    builder = builder.rule(
        RuleRow::new(
            RuleId::new(id + 1).unwrap(),
            "perm groups",
            "res.groups",
            "crud",
        )
        .unwrap()
        .with_group(gid(5)),
    );
    builder.build().unwrap()
}

async fn policy_harness() -> (Harness, Arc<dyn AccessPolicy>) {
    let open_registry = builder().registry_only().unwrap();
    let open: Arc<dyn AccessPolicy> = Arc::new(
        Compiler::new(open_registry)
            .compile(&PolicyRows::builder(0).build().unwrap())
            .unwrap(),
    );
    let swap = Arc::new(Swap(RwLock::new(Arc::clone(&open))));
    let provider: Arc<dyn PolicyProvider> = swap.clone();
    let h = builder().policy(provider).build().await.expect("harness");
    let policy: Arc<dyn AccessPolicy> = Arc::new(
        Compiler::new(Arc::clone(h.registry()))
            .compile(&policy_rows(h.registry()))
            .unwrap(),
    );
    *swap.0.write().unwrap() = Arc::clone(&policy);
    (h, policy)
}

/// The reader: internal-user group, allowed in B only, active in `active`.
fn principal(policy: &dyn AccessPolicy, reader: i64, b: i64, active: &[i64]) -> Principal {
    let groups = policy.groups(&[GroupId::new(GROUP).unwrap()]).unwrap();
    let company = |c: &i64| CompanyId::new(*c).unwrap();
    PrincipalBuilder::user(UserId::new(reader).unwrap())
        .resolution(
            Resolution::in_id_order(groups, [b].iter().map(company), NonZeroU64::MIN).unwrap(),
        )
        .companies(active.iter().map(company).collect())
        .build()
        .unwrap()
}

/// An administrator: `base.group_system` (which implies the internal-user group), allowed and
/// active in `company` only.
fn admin_principal(policy: &dyn AccessPolicy, admin: i64, company: i64) -> Principal {
    let groups = policy.groups(&[GroupId::new(5).unwrap()]).unwrap();
    let company = CompanyId::new(company).unwrap();
    PrincipalBuilder::user(UserId::new(admin).unwrap())
        .resolution(Resolution::in_id_order(groups, [company], NonZeroU64::MIN).unwrap())
        .companies(vec![company])
        .build()
        .unwrap()
}

/// The companies, groups, users and contacts of the scoped cases. `r` is B's parent, `f` a
/// company of its own; B's branch S has a portal user and a customer.
struct Scope {
    b: i64,
    r: i64,
    f: i64,
    internal: i64,
    reader: i64,
    u_int_f: i64,
    p_int_f: i64,
    u_portal_f: i64,
    u_portal_b: i64,
    u_portal_s: i64,
    p_share_f: i64,
    p_share_none: i64,
    p_share_r: i64,
    p_share_s: i64,
    p_share_b: i64,
}

async fn sql_group(tx: &mut Transaction, xmlid: &str, name: &str) -> i64 {
    let id: i64 = tx
        .query(
            "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
            &[&name],
        )
        .await
        .unwrap()[0]
        .get(0);
    bind(tx, xmlid, id).await;
    id
}

async fn company(tx: &mut Transaction, name: &str, currency: i64, parent: Option<i64>) -> i64 {
    let mut pairs = vec![("name", s(name)), ("currency_id", int(currency))];
    if let Some(parent) = parent {
        pairs.push(("parent_id", int(parent)));
    }
    create(tx, "res.company", &pairs).await
}

async fn scoped_user(
    tx: &mut Transaction,
    login: &str,
    groups: &[i64],
    company: i64,
    partner: Option<i64>,
) -> i64 {
    let mut pairs = vec![
        ("name", s(login)),
        ("login", s(login)),
        ("group_ids", set_cmd(groups)),
        ("company_id", int(company)),
        ("company_ids", set_cmd(&[company])),
    ];
    if let Some(partner) = partner {
        pairs.push(("partner_id", int(partner)));
    }
    create(tx, "res.users", &pairs).await
}

async fn contact(tx: &mut Transaction, name: &str, company: Option<i64>) -> i64 {
    let mut pairs = vec![("name", s(name))];
    if let Some(company) = company {
        pairs.push(("company_id", int(company)));
    }
    create(tx, "res.partner", &pairs).await
}

async fn seed(h: &Harness) -> Scope {
    let mut tx = h.begin().await.unwrap();
    let currency: i64 = tx
        .query(
            "INSERT INTO res_currency (name, symbol, rounding, active) \
             VALUES ('TST', 'T', 0.01, true) RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let r = company(&mut tx, "R", currency, None).await;
    let b = company(&mut tx, "B", currency, Some(r)).await;
    let s_ = company(&mut tx, "S", currency, Some(b)).await;
    let f = company(&mut tx, "F", currency, None).await;
    let internal = sql_group(&mut tx, "base.group_user", "Internal User").await;
    let portal = sql_group(&mut tx, "base.group_portal", "Portal").await;
    sql_group(&mut tx, "base.group_public", "Public").await;
    let system = sql_group(&mut tx, "base.group_system", "Administrator").await;
    scoped_user(&mut tx, "admin", &[system, internal], b, None).await;
    let reader = scoped_user(&mut tx, "reader", &[internal], b, None).await;
    let p_int_f = contact(&mut tx, "Internal of F", Some(f)).await;
    let u_int_f = scoped_user(&mut tx, "int_f", &[internal], f, Some(p_int_f)).await;
    let p_portal_f = contact(&mut tx, "Portal of F", Some(f)).await;
    let u_portal_f = scoped_user(&mut tx, "portal_f", &[portal], f, Some(p_portal_f)).await;
    let u_portal_b = scoped_user(&mut tx, "portal_b", &[portal], b, None).await;
    let u_portal_s = scoped_user(&mut tx, "portal_s", &[portal], s_, None).await;
    let p_share_f = contact(&mut tx, "Customer of F", Some(f)).await;
    let p_share_none = contact(&mut tx, "Customer of nobody", None).await;
    let p_share_r = contact(&mut tx, "Customer of R", Some(r)).await;
    let p_share_s = contact(&mut tx, "Customer of S", Some(s_)).await;
    let p_share_b = contact(&mut tx, "Customer of B", Some(b)).await;
    tx.commit().await.unwrap();
    Scope {
        b,
        r,
        f,
        internal,
        reader,
        u_int_f,
        p_int_f,
        u_portal_f,
        u_portal_b,
        u_portal_s,
        p_share_f,
        p_share_none,
        p_share_r,
        p_share_s,
        p_share_b,
    }
}

fn int_of(value: &Value) -> Option<i64> {
    match value {
        Value::Int(id) => Some(*id),
        Value::False => None,
        other => panic!("not a many2one value: {other:?}"),
    }
}

fn bool_of(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        other => panic!("not a boolean: {other:?}"),
    }
}

/// B and its ancestors, from B's stored parent path.
async fn ancestors_of(tx: &mut Transaction, company: i64) -> BTreeSet<i64> {
    let path = records(tx, "res.company", &[company])
        .values(tx, "parent_path")
        .await
        .unwrap();
    let Value::Str(path) = &path[0] else {
        panic!("no parent path: {path:?}");
    };
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().unwrap())
        .collect()
}

/// `res.partner` admits a record that is not a share partner, has no company, or belongs to an
/// active company or one of its ancestors (`branch = "ancestor"`, `empty = "allow"`).
fn partner_admitted(share: bool, company: Option<i64>, ancestors: &BTreeSet<i64>) -> bool {
    !share || company.is_none_or(|c| ancestors.contains(&c))
}

/// `res.users` admits an internal user, or a user one of whose companies is active (`exact`).
fn user_admitted(share: bool, companies: &[i64], active: &[i64]) -> bool {
    !share || companies.iter().any(|c| active.contains(c))
}

/// The partners and users the reader should see, from the stored values read as superuser.
async fn expected(h: &Harness, scope: &Scope) -> (BTreeSet<i64>, BTreeSet<i64>) {
    let mut tx = h.begin().await.unwrap();
    let ancestors = ancestors_of(&mut tx, scope.b).await;
    assert_eq!(ancestors, BTreeSet::from([scope.r, scope.b]));
    let partners = tx
        .env()
        .records("res.partner")
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    let shares = partners.values(&mut tx, "partner_share").await.unwrap();
    let companies = partners.values(&mut tx, "company_id").await.unwrap();
    let want_partners: BTreeSet<i64> = partners
        .ids()
        .into_iter()
        .zip(shares.iter().zip(&companies))
        .filter(|(_, (share, company))| {
            partner_admitted(bool_of(share), int_of(company), &ancestors)
        })
        .map(|(id, _)| id)
        .collect();
    let users = tx
        .env()
        .records("res.users")
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    let shares = users.values(&mut tx, "share").await.unwrap();
    let companies = users.values(&mut tx, "company_ids").await.unwrap();
    let want_users: BTreeSet<i64> = users
        .ids()
        .into_iter()
        .zip(shares.iter().zip(&companies))
        .filter(|(_, (share, companies))| {
            let Value::List(list) = companies else {
                panic!("company_ids is not a list: {companies:?}");
            };
            let ids: Vec<i64> = list.iter().filter_map(int_of).collect();
            user_admitted(bool_of(share), &ids, &[scope.b])
        })
        .map(|(id, _)| id)
        .collect();
    tx.rollback().await.unwrap();
    (want_partners, want_users)
}

async fn search(tx: &mut Transaction, model: &str) -> BTreeSet<i64> {
    tx.env()
        .records(model)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids()
        .into_iter()
        .collect()
}

/// The record-level access error text (`erp-security` `explain.rs`) for the reader, on a record of
/// a company the reader is not in (that company is never named).
fn denied(h: &Harness, reader: i64, op: &str, model: &str) -> String {
    let description = &h.registry().model(model).unwrap().description;
    format!(
        "Access rules protect some of these records.\n\nreader (id={reader}) has no '{op}' access to:\n- {description} ({model})\n\nAsk an administrator if you need access to these records.\n\n{}",
        CompanyHint::OutsideYourCompanies.text().unwrap()
    )
}

/// `allow`: search, read by id of a hidden contact, and a write that moves a share contact out)
#[tokio::test]
async fn contacts_are_scoped_by_company_unless_internal() {
    let (h, policy) = policy_harness().await;
    let scope = seed(&h).await;
    let (want, _) = expected(&h, &scope).await;
    // not vacuous: admitted by the escape alone, by having no company, by an ancestor; refused
    // for a foreign company and for a branch of the active company
    for id in [
        scope.p_int_f,
        scope.p_share_none,
        scope.p_share_r,
        scope.p_share_b,
    ] {
        assert!(want.contains(&id), "{id} should be admitted");
    }
    for id in [scope.p_share_f, scope.p_share_s] {
        assert!(!want.contains(&id), "{id} should be refused");
    }

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(
            &*policy,
            scope.reader,
            scope.b,
            &[scope.b],
        )))
        .await
        .unwrap();
    assert_eq!(search(&mut tx, "res.partner").await, want);
    let err = tx
        .env()
        .records("res.partner")
        .unwrap()
        .browse([scope.p_share_f])
        .unwrap()
        .read(&mut tx, &["name"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        denied(&h, scope.reader, "read", "res.partner")
    );
    tx.rollback().await.unwrap();

    // a share contact of B moved to F leaves the reader's scope: refused on the new state
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(
            &*policy,
            scope.reader,
            scope.b,
            &[scope.b],
        )))
        .await
        .unwrap();
    let err = tx
        .env()
        .records("res.partner")
        .unwrap()
        .browse([scope.p_share_b])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("company_id", erp_sdk::Cell::Id(scope.f)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        denied(&h, scope.reader, "write", "res.partner")
    );
    assert!(tx.is_poisoned());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// many2many with `exact`: internal users of another company admitted, portal users only with an
/// active company, a branch or parent company not enough)
#[tokio::test]
async fn users_are_scoped_by_their_companies_unless_internal() {
    let (h, policy) = policy_harness().await;
    let scope = seed(&h).await;
    let (_, want) = expected(&h, &scope).await;
    assert!(want.contains(&scope.u_int_f), "internal user of F");
    assert!(want.contains(&scope.u_portal_b), "portal user of B");
    assert!(!want.contains(&scope.u_portal_f), "portal user of F");
    assert!(
        !want.contains(&scope.u_portal_s),
        "portal user of B's branch"
    );

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(
            &*policy,
            scope.reader,
            scope.b,
            &[scope.b],
        )))
        .await
        .unwrap();
    assert_eq!(search(&mut tx, "res.users").await, want);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// the `res.users` restriction)
#[tokio::test]
async fn the_superuser_row_stays_visible_to_a_restricted_user() {
    let (h, policy) = policy_harness().await;
    let scope = seed(&h).await;
    let superuser = erp_orm::SUPERUSER_ID.get();
    let mut tx = h.begin().await.unwrap();
    // what the group data load of an installation runs
    erp_addon_base::group_graph::recompute_user_share(&mut tx, None)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    // Creating a company through the ORM links it into the superuser's allowed companies since
    // that has none, so the rows are removed here (committed below, before the restricted
    // transaction starts); the test keeps its purpose.
    tx.execute(
        "DELETE FROM res_company_users_rel WHERE user_id = $1",
        &[&superuser],
    )
    .await
    .unwrap();
    // not vacuous: no company, so only the escape can admit the row
    let companies = tx
        .query(
            "SELECT cid FROM res_company_users_rel WHERE user_id = $1",
            &[&superuser],
        )
        .await
        .unwrap();
    assert!(companies.is_empty());
    tx.commit().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(
            &*policy,
            scope.reader,
            scope.b,
            &[scope.b],
        )))
        .await
        .unwrap();
    // the superuser row is archived
    let users: BTreeSet<i64> = tx
        .env()
        .records("res.users")
        .unwrap()
        .with_context([("active_test", Value::Bool(false))])
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids()
        .into_iter()
        .collect();
    assert!(users.contains(&superuser), "{users:?}");
    // a portal user of another company is still refused by the same search
    assert!(!users.contains(&scope.u_portal_f), "{users:?}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// and its contact from the next restricted search of the same transaction, without a manual
/// flush, and restoring it shows both again)
#[tokio::test]
async fn a_group_change_moves_a_user_and_its_contact_out_of_scope_and_back() {
    let (h, policy) = policy_harness().await;
    let scope = seed(&h).await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(
            &*policy,
            scope.reader,
            scope.b,
            &[scope.b],
        )))
        .await
        .unwrap();
    assert!(search(&mut tx, "res.users").await.contains(&scope.u_int_f));
    assert!(
        search(&mut tx, "res.partner")
            .await
            .contains(&scope.p_int_f)
    );
    let internal = RecId::from_real(scope.internal).unwrap();
    let user = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([scope.u_int_f])
        .unwrap()
        .sudo(true);
    user.write(
        &mut tx,
        RawVals::new().set_commands("group_ids", [Command::unlink(internal)]),
    )
    .await
    .unwrap();
    assert!(!search(&mut tx, "res.users").await.contains(&scope.u_int_f));
    assert!(
        !search(&mut tx, "res.partner")
            .await
            .contains(&scope.p_int_f)
    );
    user.write(
        &mut tx,
        RawVals::new().set_commands("group_ids", [Command::link(internal)]),
    )
    .await
    .unwrap();
    assert!(search(&mut tx, "res.users").await.contains(&scope.u_int_f));
    assert!(
        search(&mut tx, "res.partner")
            .await
            .contains(&scope.p_int_f)
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// records included; a read by id of an internal user's contact is an access error)
#[tokio::test]
async fn an_empty_active_company_set_admits_no_contact_and_no_user() {
    let (h, policy) = policy_harness().await;
    let scope = seed(&h).await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(
            &*policy,
            scope.reader,
            scope.b,
            &[],
        )))
        .await
        .unwrap();
    assert!(search(&mut tx, "res.partner").await.is_empty());
    assert!(search(&mut tx, "res.users").await.is_empty());
    let err = tx
        .env()
        .records("res.partner")
        .unwrap()
        .browse([scope.p_int_f])
        .unwrap()
        .read(&mut tx, &["name"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        denied(&h, scope.reader, "read", "res.partner")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// a login and a password: the create succeeds, and the new user is internal, has B as default
/// and allowed company, and is readable to the administrator)
#[tokio::test]
async fn an_administrator_creates_an_internal_user_of_its_company() {
    let (h, policy) = policy_harness().await;
    let scope = seed(&h).await;
    let mut tx = h.begin().await.unwrap();
    let admin: i64 = tx
        .query("SELECT id FROM res_users WHERE login = 'admin'", &[])
        .await
        .unwrap()[0]
        .get(0);
    // the column `res.users` `init` adds when base is installed (this database has no install)
    tx.batch(erp_addon_base::res_users_auth::PASSWORD_COLUMN_DDL)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(admin_principal(
            &*policy, admin, scope.b,
        )))
        .await
        .unwrap();
    let created = tx
        .env()
        .records("res.users")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[
                ("name", s("New User")),
                ("login", s("new-user")),
                ("password", s("new-user-password")),
            ])],
        )
        .await
        .unwrap_or_else(|error| panic!("the administrator's create was refused: {error:?}"));
    let rows = created
        .read(&mut tx, &["share", "company_ids", "group_ids"])
        .await
        .unwrap();
    let [row] = rows.as_slice() else {
        panic!("one user expected: {rows:?}");
    };
    assert_eq!(row.get("share"), Some(&Value::Bool(false)), "{row:?}");
    assert_eq!(
        created.values(&mut tx, "company_id").await.unwrap(),
        [Value::Int(scope.b)]
    );
    assert_eq!(
        row.get("company_ids"),
        Some(&Value::list([Value::Int(scope.b)])),
        "{row:?}"
    );
    assert_eq!(
        row.get("group_ids"),
        Some(&Value::list([Value::Int(scope.internal)])),
        "{row:?}"
    );
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

/// writable by an administrator who is not the superuser; the stored value does not change)
#[tokio::test]
async fn a_non_superuser_cannot_write_the_user_type_flags() {
    let (h, policy) = policy_harness().await;
    let scope = seed(&h).await;
    let mut tx = h.begin().await.unwrap();
    let admin: i64 = tx
        .query("SELECT id FROM res_users WHERE login = 'admin'", &[])
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    for (model, field, id, stored) in [
        ("res.partner", "partner_share", scope.p_share_b, true),
        ("res.users", "share", scope.u_portal_b, true),
    ] {
        let mut tx = h
            .orm()
            .begin_opts(BeginOptions::for_principal(admin_principal(
                &*policy, admin, scope.b,
            )))
            .await
            .unwrap();
        let outcome = records(&tx, model, &[id])
            .write_values(&mut tx, &map(&[(field, Value::Bool(!stored))]))
            .await;
        let flushed = tx.flush().await;
        assert!(
            outcome.is_err() || flushed.is_err(),
            "{model}.{field} was written by a non-superuser"
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}
