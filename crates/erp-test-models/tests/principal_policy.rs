// SPDX-License-Identifier: MIT OR Apache-2.0
//! What a scoped elevation block grants under the real compiled policy (slice P2;
//! D3): a named (model, operation) counts its permission rules as granted
//! and keeps every restriction except the lifted ones; every other (model, operation) is decided
//! as the base principal's; `_inherits` parents keep their own predicate; lifted rules are checked
//! when the block opens, before it takes an identity or records anything. Every test runs the
//! policy compiled by `erp-security` from rows, against the real audit table the harness installs.
//!
//! The restricting fixture: two companies, an ordinary
//! user (2: internal, company 1) and a user with other groups (3: internal and "Other Users",
//! companies 1 and 2), a document model with ordinary, company-bearing, `access`-condition and
//! generated restrictions and a `groups='.'` field, a child that delegates to it through
//! `_inherits`, and explicit allowed and denied rows. Field grants are not part of this slice
//! : a group-restricted field stays denied in a block.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::{Arc, PoisonError, RwLock};

use erp_domain::Domain;
use erp_fields::{Error, ErrorKind, Value};
use erp_orm::audit::ElevationReason;
use erp_orm::{
    AccessPolicy, ApiKeyId, BeginOptions, BlockId, BoxFuture, CompanyId, Elevation, ElevationScope,
    Env, GroupId, MethodAllow, ModelAllow, Operation, OperationIdentity, OperationSet,
    PolicyProvider, Principal, PrincipalBuilder, QualifiedXmlId, Records, Resolution, Scope,
    ScopeItem, Transaction, UserId,
};
use erp_registry::Registry;
use erp_security::company_access::generate;
use erp_security::rows::{GroupRow, Implication, PolicyRows, RuleId, RuleRow};
use erp_security::snapshot::Compiler;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls};

mod fixture {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "prin_policy");

    /// `res.company` (this fixture is loaded without another company model).
    #[model(
        name = "res.company",
        description = "Companies",
        addon = "prin_policy",
        parent_store = true,
        parent_name = "parent_id"
    )]
    pub struct Company {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(many2one, comodel = "res.company")]
        pub parent_id: Option<RecId>,
        #[field(char, index)]
        pub parent_path: Option<Arc<str>>,
    }

    /// A tag with a company: a document rule reaches the company through it.
    #[model(
        name = "prin.policy.tag",
        description = "Policy Tag",
        addon = "prin_policy",
        company_access = unrestricted(reason = "scoped elevation fixture: hand-written rules")
    )]
    pub struct Tag {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(many2one, comodel = "res.company")]
        pub company_id: Option<RecId>,
    }

    /// The document: ordinary, company-bearing and `access` restrictions, a generated company
    /// restriction, and a field no group may read.
    #[model(
        name = "prin.policy.doc",
        description = "Policy Doc",
        addon = "prin_policy",
        company_access = direct(field = "company_id", branch = "exact", empty = "deny")
    )]
    pub struct Doc {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(integer)]
        pub level: i32,
        #[field(many2one, comodel = "prin.policy.tag")]
        pub tag_id: Option<RecId>,
        #[field(many2one, comodel = "res.company")]
        pub company_id: Option<RecId>,
        #[field(char, groups = ".")]
        pub secret: Option<Arc<str>>,
    }

    /// A model the scopes of these tests never name.
    #[model(
        name = "prin.policy.other",
        description = "Policy Other",
        addon = "prin_policy"
    )]
    pub struct Other {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }

    /// Delegates to a document (`_inherits`).
    #[model(
        name = "prin.policy.child",
        description = "Policy Child",
        addon = "prin_policy",
        inherits = [("prin.policy.doc", "doc_id")],
        company_access = unrestricted(reason = "scoped elevation fixture: the parent's rules apply")
    )]
    pub struct Child {
        #[field(many2one, comodel = "prin.policy.doc", required, ondelete = "cascade")]
        pub doc_id: Option<RecId>,
        #[field(char)]
        pub label: Option<Arc<str>>,
        #[field(integer)]
        pub size: i32,
    }
}

const DOC: &str = "prin.policy.doc";
const TAG: &str = "prin.policy.tag";
const OTHER: &str = "prin.policy.other";
const CHILD: &str = "prin.policy.child";
const REASON: &str = "prin_policy.test.block";

/// `base.group_user`.
const USER: i64 = 1;
/// "Other Users": the only group with a permission on documents.
const OTHERS: i64 = 5;
/// `base.group_no_one`: no fixture user holds it.
const NO_ONE: i64 = 4;
/// The ordinary user: internal, company 1.
const U2: i64 = 2;
/// The user with other groups: internal and "Other Users", companies 1 and 2.
const U3: i64 = 3;

// the external ids of the rules
const PERM_DOC_OTHERS: &str = "prin_policy.perm_doc_others";
const RULE_DOC_LEVEL: &str = "prin_policy.rule_doc_level";
const RULE_DOC_TAG_COMPANY: &str = "prin_policy.rule_doc_tag_company";
const RULE_DOC_COMPANY: &str = "prin_policy.rule_doc_company";
const RULE_DOC_TAG_ACCESS: &str = "prin_policy.rule_doc_tag_access";
const RULE_DOC_UNLINK_ONLY: &str = "prin_policy.rule_doc_unlink_only";
const RULE_OTHER_NAME: &str = "prin_policy.rule_other_name";
const RULE_CHILD_BROKEN: &str = "prin_policy.rule_child_broken";
const PERM_COMPANY_USER: &str = "prin_policy.perm_company_user";
const PERM_COMPANY_ALLOWED: &str = "prin_policy.perm_company_allowed";
const GENERATED_DOC: &str = "prin_policy.generated_doc";

/// The record-level refusal of user 2's write moving a document to company 2 (SEC-009 text with
/// the multi-company hint of the generated company restriction: user 2 is not in company 2).
const MOVED_TO_COMPANY_2: &str = "Access rules protect some of these records.\n\nu2 (id=2) has no 'write' access \
                                  to:\n- Policy Doc (prin.policy.doc)\n\nAsk an administrator if you need access to these records.\n\nThe record belongs to a company outside your allowed companies.";

/// The shared policy, and the policy a private rebuild returns (`None`: the rebuild is refused).
struct Swap(
    RwLock<Arc<dyn AccessPolicy>>,
    RwLock<Option<Arc<dyn AccessPolicy>>>,
);

impl PolicyProvider for Swap {
    fn current(&self) -> Arc<dyn AccessPolicy> {
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn rebuild_private<'a>(
        &'a self,
        _tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<Arc<dyn AccessPolicy>>> {
        let private = self
            .1
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Box::pin(async {
            private
                .ok_or_else(|| erp_fields::Error::programming("this test writes no policy table"))
        })
    }
}

fn gid(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn xml(text: &str) -> QualifiedXmlId {
    QualifiedXmlId::parse(text).unwrap()
}

/// One rule row: `(id, xmlid, model, operation, group, domain)`.
fn rule(
    id: i64,
    xmlid: &str,
    model: &str,
    ops: &str,
    group: Option<i64>,
    domain: Option<&str>,
) -> RuleRow {
    let mut row = RuleRow::new(RuleId::new(id).unwrap(), xmlid, model, ops)
        .unwrap()
        .with_xmlid(xml(xmlid));
    if let Some(group) = group {
        row = row.with_group(gid(group));
    }
    if let Some(domain) = domain {
        row = row.with_domain(domain);
    }
    row
}

fn rows(registry: &Registry) -> PolicyRows {
    rows_with(registry, [])
}

/// [`rows`] with `extra` rules added.
fn rows_with(registry: &Registry, extra: impl IntoIterator<Item = RuleRow>) -> PolicyRows {
    let group = |n, name: &str, x: &str| GroupRow::new(gid(n), name).unwrap().with_xmlid(xml(x));
    let mut builder = PolicyRows::builder(1)
        .group(group(USER, "Internal User", "base.group_user"))
        .group(group(2, "Everyone", "base.group_everyone"))
        .group(group(3, "Portal", "base.group_portal").with_share(true))
        .group(group(4, "Technical Features", "base.group_no_one"))
        .group(group(OTHERS, "Other Users", "prin_policy.group_others"))
        .implication(Implication::new(gid(USER), gid(2)))
        .implication(Implication::new(gid(3), gid(2)))
        .rule(rule(1, PERM_DOC_OTHERS, DOC, "crud", Some(OTHERS), None))
        .rule(rule(
            2,
            RULE_DOC_LEVEL,
            DOC,
            "crud",
            None,
            Some("[('level', '<', 10)]"),
        ))
        .rule(rule(
            3,
            RULE_DOC_TAG_COMPANY,
            DOC,
            "r",
            None,
            Some("[('tag_id.company_id', 'in', company_ids)]"),
        ))
        .rule(rule(
            4,
            RULE_DOC_COMPANY,
            DOC,
            "r",
            None,
            Some("['|', ('company_id', '=', False), ('company_id', 'in', company_ids)]"),
        ))
        .rule(rule(
            5,
            RULE_DOC_TAG_ACCESS,
            DOC,
            "r",
            None,
            Some("[('tag_id', 'access', 'read')]"),
        ))
        .rule(rule(
            6,
            RULE_DOC_UNLINK_ONLY,
            DOC,
            "d",
            None,
            Some("[('level', '<', 100)]"),
        ))
        .rule(rule(
            7,
            "prin_policy.perm_tag_user",
            TAG,
            "r",
            Some(USER),
            Some("[('name', '!=', 'hidden')]"),
        ))
        .rule(rule(
            8,
            "prin_policy.perm_other_user",
            OTHER,
            "r",
            Some(USER),
            None,
        ))
        .rule(rule(
            9,
            RULE_OTHER_NAME,
            OTHER,
            "crud",
            None,
            Some("[('name', '!=', 'x')]"),
        ))
        .rule(rule(
            10,
            "prin_policy.perm_child_user",
            CHILD,
            "r",
            Some(USER),
            None,
        ))
        .rule(rule(
            11,
            "prin_policy.rule_child_size",
            CHILD,
            "crud",
            None,
            Some("[('size', '<', 5)]"),
        ))
        .rule(rule(
            12,
            RULE_CHILD_BROKEN,
            CHILD,
            "d",
            None,
            Some("[('no_such_field', '=', 1)]"),
        ))
        // companies are read by the company checks of the engine
        .rule(rule(
            13,
            PERM_COMPANY_USER,
            "res.company",
            "r",
            Some(USER),
            None,
        ))
        // company-bearing permissions of a group no fixture user holds (A6): the
        // company's only company filter (as `base.access_company_internal`), and a document
        // permission next to the document's generated company restriction, which is kept
        .rule(rule(
            14,
            PERM_COMPANY_ALLOWED,
            "res.company",
            "ru",
            Some(NO_ONE),
            Some("[('id', 'in', company_ids)]"),
        ))
        .rule(rule(
            15,
            "prin_policy.perm_doc_company_no_one",
            DOC,
            "ru",
            Some(NO_ONE),
            Some("[('company_id', 'in', company_ids)]"),
        ));
    let generated = generate(registry).unwrap();
    let models: Vec<&str> = generated.iter().map(|g| g.model_name()).collect();
    assert_eq!(models, [DOC], "the fixture generates for the document only");
    for g in &generated {
        builder = builder.rule(
            RuleRow::new(RuleId::new(20).unwrap(), g.name(), g.model_name(), "crud")
                .unwrap()
                .with_domain(g.text())
                .with_generated()
                .with_xmlid(xml(GENERATED_DOC)),
        );
    }
    for row in extra {
        builder = builder.rule(row);
    }
    builder.build().unwrap()
}

/// The seeded ids.
#[derive(Debug, Clone, Copy)]
struct Ids {
    /// level 1, company 1, tag of company 1: what user 2's restrictions allow.
    d1: i64,
    /// level 20: excluded by the level rule.
    d2: i64,
    /// company 2: excluded by the generated and the direct company restriction for company 1.
    d3: i64,
    /// tag of company 2: excluded by the nested company rule for company 1.
    d4: i64,
    /// tag named `hidden`: excluded by the `access` rule (the tag is not readable to the base).
    d5: i64,
    /// child of d1, size 1.
    c1: i64,
    /// child of d1, size 9: excluded by the child's own restriction.
    c2: i64,
    /// child of d2, size 1: excluded by its parent's level rule.
    c3: i64,
    /// tags: company 1, company 2, `hidden` of company 1.
    tags: [i64; 3],
    /// others: `a`, `b`, and `x` (excluded by its restriction).
    others: [i64; 3],
}

impl Ids {
    fn docs(&self) -> Vec<i64> {
        vec![self.d1, self.d2, self.d3, self.d4, self.d5]
    }

    fn children(&self) -> Vec<i64> {
        vec![self.c1, self.c2, self.c3]
    }
}

async fn insert(tx: &mut Transaction, sql: &str) -> i64 {
    tx.query(sql, &[]).await.unwrap()[0].get(0)
}

async fn doc(tx: &mut Transaction, name: &str, level: i64, tag: i64, company: i64) -> i64 {
    insert(
        tx,
        &format!(
            "INSERT INTO prin_policy_doc (name, level, tag_id, company_id, secret) VALUES \
             ('{name}', {level}, {tag}, {company}, 's') RETURNING id"
        ),
    )
    .await
}

async fn child(tx: &mut Transaction, doc: i64, size: i64) -> i64 {
    insert(
        tx,
        &format!(
            "INSERT INTO prin_policy_child (doc_id, label, size) VALUES ({doc}, 'c', {size}) \
             RETURNING id"
        ),
    )
    .await
}

/// The harness deciding with the compiled policy of [`rows`], and the seeded rows (as the
/// superuser, with SQL: the rows are fixture data, not under test).
async fn harness() -> (Harness, Arc<dyn AccessPolicy>, Ids) {
    let (h, policy, ids, _swap) = harness_swap().await;
    (h, policy, ids)
}

/// [`harness`] and its policy provider, whose private rebuild a test may set.
async fn harness_swap() -> (Harness, Arc<dyn AccessPolicy>, Ids, Arc<Swap>) {
    let open: Arc<dyn AccessPolicy> = Arc::new(
        Compiler::new(Arc::new(
            erp_registry::RegistryBuilder::new([&fixture::ADDON])
                .build()
                .unwrap(),
        ))
        .compile(&PolicyRows::builder(0).build().unwrap())
        .unwrap(),
    );
    let swap = Arc::new(Swap(RwLock::new(Arc::clone(&open)), RwLock::new(None)));
    let provider: Arc<dyn PolicyProvider> = swap.clone();
    let h = Harness::builder([&fixture::ADDON])
        .policy(provider)
        .build()
        .await
        .expect("harness");
    let snapshot = Compiler::new(Arc::clone(h.registry()))
        .compile(&rows(h.registry()))
        .unwrap();
    // the fixture's rules compile, except the deliberately broken one, and nothing drifted
    let report = snapshot.report();
    let broken: Vec<&str> = report
        .rule_errors()
        .iter()
        .map(|issue| issue.rule().name())
        .collect();
    assert_eq!(broken, [RULE_CHILD_BROKEN]);
    assert!(
        report.generated_issues().is_empty(),
        "{:?}",
        report.generated_issues()
    );
    let policy: Arc<dyn AccessPolicy> = Arc::new(snapshot);
    *swap.0.write().unwrap() = Arc::clone(&policy);
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_company (id, name, parent_id, parent_path) VALUES \
         (1, 'C1', NULL, '1/'), (2, 'C2', NULL, '2/'); \
         SELECT setval(pg_get_serial_sequence('res_company', 'id'), 2); \
         INSERT INTO res_users (id, name, login, active) VALUES \
         (2, 'u2', 'u2', true), (3, 'u3', 'u3', true)",
    )
    .await
    .unwrap();
    let t1 = insert(
        &mut tx,
        "INSERT INTO prin_policy_tag (name, company_id) VALUES ('t1', 1) RETURNING id",
    )
    .await;
    let t2 = insert(
        &mut tx,
        "INSERT INTO prin_policy_tag (name, company_id) VALUES ('t2', 2) RETURNING id",
    )
    .await;
    let t3 = insert(
        &mut tx,
        "INSERT INTO prin_policy_tag (name, company_id) VALUES ('hidden', 1) RETURNING id",
    )
    .await;
    let d1 = doc(&mut tx, "d1", 1, t1, 1).await;
    let d2 = doc(&mut tx, "d2", 20, t1, 1).await;
    let d3 = doc(&mut tx, "d3", 1, t1, 2).await;
    let d4 = doc(&mut tx, "d4", 1, t2, 1).await;
    let d5 = doc(&mut tx, "d5", 1, t3, 1).await;
    let c1 = child(&mut tx, d1, 1).await;
    let c2 = child(&mut tx, d1, 9).await;
    let c3 = child(&mut tx, d2, 1).await;
    let mut others = [0; 3];
    for (slot, name) in others.iter_mut().zip(["a", "b", "x"]) {
        *slot = insert(
            &mut tx,
            &format!("INSERT INTO prin_policy_other (name) VALUES ('{name}') RETURNING id"),
        )
        .await;
    }
    tx.commit().await.unwrap();
    let ids = Ids {
        d1,
        d2,
        d3,
        d4,
        d5,
        c1,
        c2,
        c3,
        tags: [t1, t2, t3],
        others,
    };
    (h, policy, ids, swap)
}

/// User 2 (internal, company 1) or user 3 (internal and "Other Users", companies 1 and 2), with
/// all their companies active, resolved under `policy`.
fn principal(policy: &dyn AccessPolicy, uid: i64) -> Principal {
    principal_with(policy, uid, None)
}

fn principal_with(policy: &dyn AccessPolicy, uid: i64, key: Option<Scope>) -> Principal {
    let (direct, companies): (&[i64], &[i64]) = match uid {
        U2 => (&[USER], &[1]),
        U3 => (&[USER, OTHERS], &[1, 2]),
        other => panic!("the fixture has no user {other}"),
    };
    let direct: Vec<GroupId> = direct.iter().map(|g| gid(*g)).collect();
    let groups = policy.groups(&direct).unwrap();
    let company = |c: &i64| CompanyId::new(*c).unwrap();
    let mut builder = PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(
            Resolution::in_id_order(groups, companies.iter().map(company), NonZeroU64::MIN)
                .unwrap(),
        )
        .companies(companies.iter().map(company).collect());
    if let Some(key) = key {
        builder = builder.scope(key);
    }
    builder.build().unwrap()
}

async fn begin(h: &Harness, principal: Principal) -> Transaction {
    h.orm()
        .begin_opts(
            BeginOptions::for_principal(principal)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}

fn reason() -> ElevationReason {
    ElevationReason::new(REASON).unwrap()
}

fn model(tx: &Transaction, name: &str) -> erp_registry::ModelId {
    tx.registry().model_id(name).unwrap()
}

fn item(tx: &Transaction, name: &str, ops: &[Operation]) -> ScopeItem {
    ScopeItem::new(model(tx, name), OperationSet::of(ops.iter().copied())).unwrap()
}

fn lifting(tx: &Transaction, name: &str, ops: &[Operation], lift: &[&str]) -> ScopeItem {
    item(tx, name, ops)
        .lift(lift.iter().map(|x| xml(x)))
        .unwrap()
}

fn scope(items: impl IntoIterator<Item = ScopeItem>) -> ElevationScope {
    ElevationScope::new(items).unwrap()
}

fn vals(items: &[(&str, Value)]) -> BTreeMap<String, Value> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

#[track_caller]
fn assert_error(error: &Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind, "{error:?}");
    assert_eq!(error.message(), message);
}

/// `actual` is `expected`, an access refusal (kind and message).
#[track_caller]
fn same_refusal(actual: &Error, expected: &Error) {
    assert_eq!(expected.kind(), ErrorKind::Access, "{expected:?}");
    assert_eq!(actual.kind(), expected.kind(), "{actual:?}");
    assert_eq!(actual.message(), expected.message());
}

fn block_of(env: &Env) -> BlockId {
    match env.principal().elevation() {
        Elevation::Scoped { block, .. } => *block,
        other => panic!("not a scoped environment: {other:?}"),
    }
}

/// The ids of `model` that `env` finds with an empty domain.
async fn found(tx: &mut Transaction, env: &Env, model: &str) -> erp_fields::Result<Vec<i64>> {
    let mut ids = env.records(model)?.search(tx, &Domain::TRUE).await?.ids();
    ids.sort_unstable();
    Ok(ids)
}

/// Which of `ids` pass the record check of `op` for `env`, one id at a time. Only an `Access`
/// error counts as a denial: any other error fails the test with the error shown.
async fn checked(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: &[i64],
    op: Operation,
) -> Vec<i64> {
    let mut out = Vec::new();
    for id in ids {
        let one = env.records(model).unwrap().browse([*id]).unwrap();
        // only an access refusal is a denial; any other error is a failure of the test
        match one.check_access(tx, op).await {
            Ok(()) => out.push(*id),
            Err(error) if error.kind() == ErrorKind::Access => {}
            Err(error) => panic!("check_access of {model} {id} for {op}: {error:?}"),
        }
    }
    out
}

fn browse(env: &Env, model: &str, ids: &[i64]) -> Records {
    env.records(model)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}

/// An owner connection (the owner role holds every right on the audit table).
async fn owner(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    client
}

/// Every elevation event committed so far, in the order of the audit rows.
async fn elevation_events(h: &Harness) -> Vec<serde_json::Value> {
    let rows = owner(h)
        .await
        .query("SELECT events::text FROM erp_event.ir_audit_entry", &[])
        .await
        .unwrap();
    let mut out = Vec::new();
    for row in rows {
        let text: String = row.get(0);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        out.extend(
            value
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["type"] == "elevation")
                .cloned(),
        );
    }
    out
}

/// The error the base environment of the transaction gets for `op` on `ids` of `model` (a search
/// for `Read`; for the others, the operation on the browsed ids).
async fn base_refusal(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: &[i64],
    op: Operation,
) -> Error {
    attempt(tx, env, model, ids, op)
        .await
        .expect_err("the base principal is refused")
}

/// `op` on `ids` of `model` as `env`: a search for `Read`, else the operation itself (a create
/// makes one record with a name).
async fn attempt(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: &[i64],
    op: Operation,
) -> erp_fields::Result<()> {
    match op {
        Operation::Read => found(tx, env, model).await.map(drop),
        Operation::Write => {
            browse(env, model, ids)
                .write_values(tx, &vals(&[("name", Value::str("changed"))]))
                .await
        }
        Operation::Create => env
            .records(model)?
            .create_values(tx, &[vals(&[("name", Value::str("new"))])])
            .await
            .map(drop),
        Operation::Unlink => browse(env, model, ids).unlink(tx).await,
    }
}

/// the rows the restrictions allow (no permission row of user 2's groups grants it); Write,
/// Create and Unlink of the document and a write of the unrelated model are refused with the
/// base principal's own kind and message; the unrelated model's read, which the base allows,
/// returns what it returns outside the block; a user whose base already reads the document reads
/// the same rows in the block. The `groups='.'` field stays closed.
#[tokio::test]
async fn scope_admits_only_named_models_and_operations() {
    let (h, policy, ids) = harness().await;
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();
    // the base principal: no permission on documents, reads the unrelated model
    let mut base_errors = BTreeMap::new();
    for op in Operation::ALL {
        let error = base_refusal(&mut tx, &base, DOC, &[ids.d1], op).await;
        base_errors.insert(op.as_str(), error);
    }
    let other_write = base_refusal(&mut tx, &base, OTHER, &ids.others, Operation::Write).await;
    let base_others = found(&mut tx, &base, OTHER).await.unwrap();
    assert_eq!(base_others, vec![ids.others[0], ids.others[1]]);
    let s = scope([item(&tx, DOC, &[Operation::Read])]);
    let (docs, checked_docs, errors, other_error, others) = tx
        .elevated(&base, reason(), s, async |tx, env| {
            let docs = found(tx, &env, DOC).await?;
            let checked_docs = checked(tx, &env, DOC, &ids.docs(), Operation::Read).await;
            let mut errors = BTreeMap::new();
            for op in [Operation::Write, Operation::Create, Operation::Unlink] {
                let error = attempt(tx, &env, DOC, &[ids.d1], op)
                    .await
                    .expect_err("an unnamed operation keeps the base decision");
                errors.insert(op.as_str(), error);
            }
            let other_error = attempt(tx, &env, OTHER, &ids.others, Operation::Write)
                .await
                .expect_err("the unrelated model keeps the base decision");
            let others = found(tx, &env, OTHER).await?;
            Ok((docs, checked_docs, errors, other_error, others))
        })
        .await
        .unwrap();
    assert_eq!(docs, vec![ids.d1], "the restrictions decide the named read");
    assert_eq!(
        checked_docs,
        vec![ids.d1],
        "the record check agrees with search"
    );
    for (op, error) in &errors {
        same_refusal(error, &base_errors[op]);
    }
    same_refusal(&other_error, &other_write);
    assert_eq!(
        others, base_others,
        "an operation the base allows still succeeds"
    );
    // the base decision is back after the block
    same_refusal(
        &base_refusal(&mut tx, &base, DOC, &[ids.d1], Operation::Read).await,
        &base_errors["read"],
    );
    drop(tx);

    // the user whose groups read documents reads the same rows in the block (companies 1 and 2)
    let mut tx = begin(&h, principal(&*policy, U3)).await;
    let base = tx.env();
    let outside = found(&mut tx, &base, DOC).await.unwrap();
    assert_eq!(outside, vec![ids.d1, ids.d3, ids.d4]);
    let secret_base = browse(&base, DOC, &[ids.d1])
        .value(&mut tx, "secret")
        .await
        .expect_err("no group reads the secret");
    let s = scope([item(&tx, DOC, &[Operation::Read])]);
    let inside = tx
        .elevated(&base, reason(), s, async |tx, env| {
            found(tx, &env, DOC).await
        })
        .await
        .unwrap();
    assert_eq!(inside, outside);
    drop(tx);

    // the field mask is not widened by a scope: user 2 reads the document
    // in the block, not its closed field
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();
    let s = scope([item(&tx, DOC, &[Operation::Read])]);
    let (name, secret) = tx
        .elevated(&base, reason(), s, async |tx, env| {
            let doc = browse(&env, DOC, &[ids.d1]);
            let name = doc.value(tx, "name").await?;
            let secret = doc.value(tx, "secret").await.expect_err("closed field");
            Ok((name, secret))
        })
        .await
        .unwrap();
    assert_eq!(name, Value::str("d1"));
    same_refusal(&secret, &secret_base);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `access` and generated ones), in a read and a write; a child whose parent is not named keeps
/// the base decision; a named parent keeps its restrictions; a valid lift of an ordinary
/// restriction widens only inside its block; lifting the company restriction nested in a
/// relational path is `Programming` naming the rule, before the body runs, without taking a block
/// identity or recording evidence.
#[tokio::test]
async fn restrictions_and_parents_remain_authoritative() {
    let (h, policy, ids) = harness().await;
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();

    // a refused nested-company lift: before the body, no block identity, no evidence
    let mut ran = false;
    let refused = tx
        .elevated(
            &base,
            reason(),
            scope([lifting(
                &tx,
                DOC,
                &[Operation::Read],
                &[RULE_DOC_TAG_COMPANY],
            )]),
            async |_tx, _env| {
                ran = true;
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert_error(
        &refused,
        ErrorKind::Programming,
        &format!(
            "cannot lift restriction rule '{RULE_DOC_TAG_COMPANY}' ({RULE_DOC_TAG_COMPANY}) on \
             {DOC}: it bears the company (its domain names the company), and a scope never lifts \
             a company restriction"
        ),
    );
    assert!(!ran, "the refused body never ran");

    // unlifted restrictions exclude, in a read and in a write
    let s = scope([item(&tx, DOC, &[Operation::Read, Operation::Write])]);
    let (first, read, write_ok, write_denied) = tx
        .elevated(&base, reason(), s, async |tx, env| {
            let first = block_of(&env);
            let read = found(tx, &env, DOC).await?;
            browse(&env, DOC, &[ids.d1])
                .write_values(tx, &vals(&[("name", Value::str("d1 edited"))]))
                .await?;
            let write_ok = checked(tx, &env, DOC, &ids.docs(), Operation::Write).await;
            let write_denied = browse(&env, DOC, &[ids.d2])
                .write_values(tx, &vals(&[("name", Value::str("no"))]))
                .await
                .expect_err("the level rule excludes the write");
            Ok((first, read, write_ok, write_denied))
        })
        .await
        .unwrap();
    assert_eq!(first.seq(), 1, "the refused entry took no block identity");
    assert_eq!(read, vec![ids.d1]);
    // the write rules are the level and the generated company restriction only
    assert_eq!(write_ok, vec![ids.d1, ids.d4, ids.d5]);
    assert_error(
        &write_denied,
        ErrorKind::Access,
        "Access rules protect some of these records.\n\nu2 (id=2) has no 'write' access to:\n- Policy Doc (prin.policy.doc)\n\nAsk an administrator if you need access to these records.",
    );

    // a child whose parent is not named keeps the base decision (the parent denies everything)
    let child_base = base_refusal(&mut tx, &base, CHILD, &[], Operation::Read).await;
    let s = scope([item(&tx, CHILD, &[Operation::Read])]);
    let unnamed_parent = tx
        .elevated(&base, reason(), s, async |tx, env| {
            Ok(found(tx, &env, CHILD)
                .await
                .expect_err("the unnamed parent denies"))
        })
        .await
        .unwrap();
    same_refusal(&unnamed_parent, &child_base);

    // a named parent keeps its restrictions: c2 fails its own rule, c3's parent the level rule
    let s = scope([
        item(&tx, CHILD, &[Operation::Read]),
        item(&tx, DOC, &[Operation::Read]),
    ]);
    let named_parent = tx
        .elevated(&base, reason(), s, async |tx, env| {
            let found = found(tx, &env, CHILD).await?;
            let checked = checked(tx, &env, CHILD, &ids.children(), Operation::Read).await;
            Ok((found, checked))
        })
        .await
        .unwrap();
    assert_eq!(named_parent, (vec![ids.c1], vec![ids.c1]));

    // a valid lift of the ordinary level rule widens the read inside its block only
    let lifted = scope([
        item(&tx, CHILD, &[Operation::Read]),
        lifting(&tx, DOC, &[Operation::Read], &[RULE_DOC_LEVEL]),
    ]);
    let (docs, children, write_still) = tx
        .elevated(&base, reason(), lifted, async |tx, env| {
            let docs = found(tx, &env, DOC).await?;
            let children = found(tx, &env, CHILD).await?;
            let write_still = attempt(tx, &env, DOC, &[ids.d2], Operation::Write)
                .await
                .expect_err("the lift is for the read; the write is not named");
            Ok((docs, children, write_still))
        })
        .await
        .unwrap();
    assert_eq!(docs, vec![ids.d1, ids.d2]);
    assert_eq!(
        children,
        vec![ids.c1, ids.c3],
        "the named parent's lift reaches its children"
    );
    same_refusal(
        &write_still,
        &base_refusal(&mut tx, &base, DOC, &[ids.d2], Operation::Write).await,
    );
    // after the block: the base again, and a block without the lift does not see d2
    base_refusal(&mut tx, &base, DOC, &[], Operation::Read).await;
    let s = scope([item(&tx, DOC, &[Operation::Read])]);
    let again = tx
        .elevated(&base, reason(), s, async |tx, env| {
            let found = found(tx, &env, DOC).await?;
            let checked = checked(tx, &env, DOC, &[ids.d2], Operation::Read).await;
            Ok((found, checked))
        })
        .await
        .unwrap();
    assert_eq!(again, (vec![ids.d1], vec![]));
    tx.commit().await.unwrap();

    // the evidence holds the five admitted blocks (five distinct scopes), once each: the refused
    // entry is not an elevation event
    let events = elevation_events(&h).await;
    assert_eq!(events.len(), 5, "{events:?}");
    assert!(events.iter().all(|e| e["count"] == 1), "{events:?}");
    h.cleanup().await.unwrap();
}

/// in a block naming Write, and a row a lift let the block read is not served to the base
/// principal from the value cache after the block)
#[tokio::test]
async fn company_and_cached_values_stay_authoritative_around_a_block() {
    let (h, policy, ids) = harness().await;
    // moving a document to company 2, outside user 2's companies, in a block naming Write (d4:
    // its tag is of company 2, so the move is consistent and only the restriction refuses it)
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();
    let s = scope([item(&tx, DOC, &[Operation::Read, Operation::Write])]);
    let moved = tx
        .elevated(&base, reason(), s, async |tx, env| {
            Ok(browse(&env, DOC, &[ids.d4])
                .write_values(tx, &vals(&[("company_id", Value::Int(2))]))
                .await
                .expect_err("the generated company restriction refuses the new state"))
        })
        .await;
    let moved = match moved {
        Ok(error) | Err(error) => error,
    };
    assert_error(&moved, ErrorKind::Access, MOVED_TO_COMPANY_2);
    drop(tx);
    let mut check = h.begin().await.unwrap();
    let company: i64 = check
        .query(
            "SELECT company_id FROM prin_policy_doc WHERE id = $1",
            &[&ids.d4],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(company, 1, "the move was not stored");
    drop(check);

    // a value read in a block with a lift, then by the base principal from a warm cache
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();
    let cold = browse(&base, DOC, &[ids.d2])
        .value(&mut tx, "name")
        .await
        .expect_err("the base principal does not read d2");
    let s = scope([lifting(&tx, DOC, &[Operation::Read], &[RULE_DOC_LEVEL])]);
    let inside = tx
        .elevated(&base, reason(), s, async |tx, env| {
            browse(&env, DOC, &[ids.d2]).value(tx, "name").await
        })
        .await
        .unwrap();
    assert_eq!(inside, Value::str("d2"));
    let warm = browse(&base, DOC, &[ids.d2])
        .value(&mut tx, "name")
        .await
        .expect_err("the cache does not grant the base principal what the block read");
    same_refusal(&warm, &cold);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// identity or evidence; the whole case is `restrictions_and_parents_remain_authoritative`)
/// lifting the direct company restriction, the generated one, a permission, a restriction of
/// another model, an unknown external id, a rule with an `access` condition, a rule that does
/// not compile and a rule of none of the item's operations is `Programming` naming the rule and
/// why.
#[tokio::test]
async fn forbidden_lifts_are_refused_before_the_body() {
    let (h, policy, _ids) = harness().await;
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();
    let read = [Operation::Read];
    let cases: Vec<(ScopeItem, String)> = vec![
        (
            lifting(&tx, DOC, &read, &[RULE_DOC_COMPANY]),
            format!(
                "cannot lift restriction rule '{RULE_DOC_COMPANY}' ({RULE_DOC_COMPANY}) on {DOC}: \
                 it bears the company (its domain names the company), and a scope never lifts a \
                 company restriction"
            ),
        ),
        (
            lifting(&tx, DOC, &read, &[GENERATED_DOC]),
            format!(
                "cannot lift restriction rule '{GENERATED_DOC}' (Company restriction of {DOC} \
                 (company_id, exact, deny)) on {DOC}: it bears the company (it is the generated \
                 company restriction), and a scope never lifts a company restriction"
            ),
        ),
        (
            lifting(&tx, DOC, &read, &[PERM_DOC_OTHERS]),
            format!(
                "cannot lift restriction rule '{PERM_DOC_OTHERS}' ({PERM_DOC_OTHERS}) on {DOC}: \
                 it is a permission of a group, not a restriction (a scope already grants the \
                 permission of the operations it names)"
            ),
        ),
        (
            lifting(&tx, DOC, &read, &[RULE_OTHER_NAME]),
            format!(
                "cannot lift restriction rule '{RULE_OTHER_NAME}' ({RULE_OTHER_NAME}) on {DOC}: \
                 it is a rule of {OTHER}, not of the item's model"
            ),
        ),
        (
            lifting(&tx, DOC, &read, &["prin_policy.no_such_rule"]),
            format!(
                "cannot lift restriction rule 'prin_policy.no_such_rule' on {DOC}: no access rule \
                 of this policy has that external id"
            ),
        ),
        (
            lifting(&tx, DOC, &read, &[RULE_DOC_TAG_ACCESS]),
            format!(
                "cannot lift restriction rule '{RULE_DOC_TAG_ACCESS}' ({RULE_DOC_TAG_ACCESS}) on \
                 {DOC}: its domain holds an 'access' condition or a declared rule operator, which \
                 brings in another model's whole predicate, company restriction included"
            ),
        ),
        (
            lifting(&tx, CHILD, &[Operation::Unlink], &[RULE_CHILD_BROKEN]),
            format!(
                "cannot lift restriction rule '{RULE_CHILD_BROKEN}' ({RULE_CHILD_BROKEN}) on \
                 {CHILD}: the rule does not compile, and lifting it would turn its denial into an \
                 allow"
            ),
        ),
        (
            lifting(&tx, DOC, &read, &[RULE_DOC_UNLINK_ONLY]),
            format!(
                "cannot lift restriction rule '{RULE_DOC_UNLINK_ONLY}' ({RULE_DOC_UNLINK_ONLY}) on \
                 {DOC}: it applies to none of the operations the item names (read), so lifting it \
                 changes nothing"
            ),
        ),
    ];
    let mut ran = false;
    for (item, message) in cases {
        let error = tx
            .elevated(&base, reason(), scope([item]), async |_tx, _env| {
                ran = true;
                Ok(())
            })
            .await
            .unwrap_err();
        assert_error(&error, ErrorKind::Programming, &message);
    }
    assert!(!ran, "no refused body ran");
    // no refused entry took a block identity
    let s = scope([item(&tx, OTHER, &read)]);
    let first = tx
        .elevated(&base, reason(), s, async |_tx, env| Ok(block_of(&env)))
        .await
        .unwrap();
    assert_eq!(first.seq(), 1);
    tx.commit().await.unwrap();
    // and none was recorded: the one event is the admitted block's
    let events = elevation_events(&h).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["count"], 1);
    assert_eq!(events[0]["grant"]["items"][0]["model"], OTHER);
    h.cleanup().await.unwrap();
}

/// A6; a fail-closed rule of this slice, accepted) a scope naming
/// Read or Write on `res.company`, whose permissions bear the company (one is the company filter
/// `[('id', 'in', company_ids)]`, as `base.access_company_internal`) and which has no company
/// restriction, is `Programming` naming the operation, the model and the permissions, before the
/// body runs, without a block identity or evidence, even though user 2 holds none of the
/// refusing permission's group; the document, whose company-bearing permission sits next to
/// its kept company restrictions (the generated one for Write), is admitted.
#[tokio::test]
async fn company_confinement_only_in_permissions_is_refused_before_the_body() {
    let (h, policy, ids) = harness().await;
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();
    let refused = |op: &str, rules: &str| {
        format!(
            "the scope names {op} on 'res.company', which keeps no generated company \
             restriction and whose permission rules ({rules}) may bear the company: scoped \
             elevation cannot open it"
        )
    };
    let cases = vec![
        (
            item(&tx, "res.company", &[Operation::Read]),
            refused(
                "read",
                &format!("{PERM_COMPANY_USER}, {PERM_COMPANY_ALLOWED}"),
            ),
        ),
        (
            item(&tx, "res.company", &[Operation::Write]),
            refused("write", PERM_COMPANY_ALLOWED),
        ),
    ];
    let mut ran = false;
    for (item, message) in cases {
        let error = tx
            .elevated(&base, reason(), scope([item]), async |_tx, _env| {
                ran = true;
                Ok(())
            })
            .await
            .unwrap_err();
        assert_error(&error, ErrorKind::Programming, &message);
    }
    assert!(!ran, "no refused body ran");
    let s = scope([item(&tx, DOC, &[Operation::Read, Operation::Write])]);
    policy.check_scope(&s).unwrap();
    let (first, docs) = tx
        .elevated(&base, reason(), s, async |tx, env| {
            Ok((block_of(&env), found(tx, &env, DOC).await?))
        })
        .await
        .unwrap();
    assert_eq!(first.seq(), 1, "no refused entry took a block identity");
    assert_eq!(docs, vec![ids.d1]);
    tx.commit().await.unwrap();
    let events = elevation_events(&h).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["grant"]["items"][0]["model"], DOC);
    h.cleanup().await.unwrap();
}

/// transaction, through a real block, A5) a scope naming Read on the unrelated model is
/// admitted; the policy then changes in the transaction (a rebuild adds a company-bearing
/// permission of that model and operation, so the pair has no kept company confinement), and the
/// same scope at the next block entry is refused with the rebuilt policy's own error, before
/// the body, without a block identity or evidence.
#[tokio::test]
async fn a_policy_change_between_two_block_entries_is_seen_by_the_second() {
    let (h, policy, _ids, swap) = harness_swap().await;
    let rebuilt: Arc<dyn AccessPolicy> = Arc::new(
        Compiler::new(Arc::clone(h.registry()))
            .compile(&rows_with(
                h.registry(),
                [rule(
                    16,
                    "prin_policy.perm_other_company_no_one",
                    OTHER,
                    "r",
                    Some(NO_ONE),
                    Some("[('id', 'in', company_ids)]"),
                )],
            ))
            .unwrap(),
    );
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();
    let first = tx
        .elevated(
            &base,
            reason(),
            scope([item(&tx, OTHER, &[Operation::Read])]),
            async |_tx, env| Ok(block_of(&env)),
        )
        .await
        .unwrap();
    assert_eq!(first.seq(), 1);
    *swap.1.write().unwrap() = Some(Arc::clone(&rebuilt));
    tx.mark_policy_dirty();
    let s = scope([item(&tx, OTHER, &[Operation::Read])]);
    let expected = rebuilt.check_scope(&s).unwrap_err();
    let mut ran = false;
    let error = tx
        .elevated(&base, reason(), s, async |_tx, _env| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Programming, expected.message());
    assert!(!ran, "the refused body did not run");
    // the refused entry took no block identity
    let next = tx
        .elevated(
            &base,
            reason(),
            scope([item(&tx, DOC, &[Operation::Read])]),
            async |_tx, env| Ok(block_of(&env)),
        )
        .await
        .unwrap();
    assert_eq!(next.seq(), 2);
    tx.commit().await.unwrap();
    // and recorded nothing: the two events are the admitted blocks'
    let events = elevation_events(&h).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["grant"]["items"][0]["model"], OTHER);
    assert_eq!(events[1]["grant"]["items"][0]["model"], DOC);
    h.cleanup().await.unwrap();
}

/// the base principal, so a scope naming the condition's target too does not widen it) the
/// hidden tag is readable in a block naming the tag, and the document whose tag it is stays
/// excluded by the document's `access` rule.
#[tokio::test]
async fn access_conditions_of_named_models_resolve_with_the_base_principal() {
    let (h, policy, ids) = harness().await;
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();
    let base_tags = found(&mut tx, &base, TAG).await.unwrap();
    assert_eq!(base_tags, vec![ids.tags[0], ids.tags[1]]);
    let s = scope([
        item(&tx, DOC, &[Operation::Read]),
        item(&tx, TAG, &[Operation::Read]),
    ]);
    let (tags, docs) = tx
        .elevated(&base, reason(), s, async |tx, env| {
            let tags = found(tx, &env, TAG).await?;
            let docs = found(tx, &env, DOC).await?;
            Ok((tags, docs))
        })
        .await
        .unwrap();
    assert_eq!(
        tags,
        ids.tags.to_vec(),
        "the named tag read has no permission filter"
    );
    assert_eq!(
        docs,
        vec![ids.d1],
        "d5's tag is still unreadable to the base principal"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// compiled policy) an inner block naming another model drops the outer block's grant: inside
/// it the outer model is decided as the base principal's; the outer environment keeps its grant.
#[tokio::test]
async fn nested_blocks_decide_with_the_inner_scope_only() {
    let (h, policy, ids) = harness().await;
    let mut tx = begin(&h, principal(&*policy, U2)).await;
    let base = tx.env();
    let base_error = base_refusal(&mut tx, &base, DOC, &[], Operation::Read).await;
    let outer_scope = scope([item(&tx, DOC, &[Operation::Read])]);
    let inner_scope = scope([item(&tx, OTHER, &[Operation::Read])]);
    let (inner_error, outer_again) = tx
        .elevated(&base, reason(), outer_scope, async |tx, outer| {
            assert_eq!(found(tx, &outer, DOC).await?, vec![ids.d1]);
            let inner_error = tx
                .elevated(&outer, reason(), inner_scope, async |tx, inner| {
                    Ok(found(tx, &inner, DOC)
                        .await
                        .expect_err("the inner scope does not name the document"))
                })
                .await?;
            let outer_again = found(tx, &outer, DOC).await?;
            Ok((inner_error, outer_again))
        })
        .await
        .unwrap();
    same_refusal(&inner_error, &base_error);
    assert_eq!(outer_again, vec![ids.d1]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Write) the named read works, the write is refused with the read-only key error.
#[tokio::test]
async fn read_only_keys_stay_read_only_in_a_scope_naming_write() {
    let (h, policy, ids) = harness().await;
    let key = Scope::builder(ApiKeyId::new(7).unwrap(), "read only")
        .models(ModelAllow::All)
        .methods(MethodAllow::All)
        .read_only(true)
        .build()
        .unwrap();
    let mut tx = begin(&h, principal_with(&*policy, U2, Some(key))).await;
    let base = tx.env();
    let s = scope([item(&tx, DOC, &[Operation::Read, Operation::Write])]);
    let (docs, refused) = tx
        .elevated(&base, reason(), s, async |tx, env| {
            let docs = found(tx, &env, DOC).await?;
            let refused = attempt(tx, &env, DOC, &[ids.d1], Operation::Write)
                .await
                .expect_err("a read-only key never writes");
            Ok((docs, refused))
        })
        .await
        .unwrap();
    assert_eq!(docs, vec![ids.d1]);
    assert_error(
        &refused,
        ErrorKind::Access,
        "The API key 'read only' is read-only: it may not write 'Policy Doc' (prin.policy.doc).",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
