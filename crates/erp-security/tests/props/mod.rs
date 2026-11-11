//! Shared fixture of the access-engine property tests (design note §8.2):
//! a small schema covering every field type the rule compiler lowers, a seeding step that
//! rewrites its rows for each generated case, policies compiled from generated `ir.access` rows,
//! and resolved user principals.
//!
//! The schema is static: tables cannot be created per case within a CI budget, so the "random
//! model" of §8.2 is a model with 8 scalar fields, 4 many2one fields and an `_inherits` child
//! whose generated rules, rows and principals vary per case.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers"
)]

pub mod db;
pub mod generate;
pub mod oracle;
mod persistence;

use std::num::NonZeroU64;
use std::sync::{Arc, PoisonError, RwLock};

use erp_fields::OnDelete;
use erp_orm::{
    AccessPolicy, BoxFuture, CompanyId, GroupId, GroupSet, PolicyProvider, Principal,
    PrincipalBinding, PrincipalBuilder, QualifiedXmlId, Resolution, Transaction, UserId,
};
use erp_registry::{AddonDescriptor, CompanyAccessDecl, FieldDecl, ModelDecl, Registry};
use erp_security::rows::{GroupRow, Implication, PolicyRows, RuleId, RuleRow};
use erp_security::snapshot::{Compiler, PolicySnapshot};
use erp_testkit::Harness;
use proptest::test_runner::{Config, RngAlgorithm, RngSeed, TestCaseError, TestRunner};

use generate::{Case, PrincipalSpec, RuleSpec};

pub const PARTNER: &str = "x.partner";
pub const CATEG: &str = "x.categ";
pub const OBJ: &str = "x.obj";
pub const CHILD: &str = "x.child";

/// `base.group_user` (the internal user type).
pub const USER: i64 = 1;
pub const G1: i64 = 2;
pub const G2: i64 = 3;
/// Implies [`G1`].
pub const G3: i64 = 4;
/// `base.group_everyone`, implied by both user types.
pub const EVERYONE: i64 = 5;
/// `base.group_portal` (a share user type).
pub const PORTAL: i64 = 6;
/// `base.group_no_one` (the debug listing of record-level errors).
pub const NO_ONE: i64 = 7;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "x_props",
    depends: &["base"],
    exact_only: false,
};

const PARTNER_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::many2one("commercial_partner_id", PARTNER),
];

const USER_FIELDS: &[FieldDecl] = &[FieldDecl::many2one("partner_id", PARTNER)];

const CATEG_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::integer("seq"),
    FieldDecl::many2one("company_id", "res.company"),
];

const OBJ_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::char("code"),
    FieldDecl::integer("val"),
    FieldDecl::integer("qty"),
    FieldDecl::float("amount"),
    FieldDecl::boolean("flag"),
    FieldDecl::date("day"),
    FieldDecl::datetime("moment"),
    FieldDecl::many2one("categ_id", CATEG),
    FieldDecl::many2one("partner_id", PARTNER),
    FieldDecl::many2one("company_id", "res.company"),
    FieldDecl::many2one("user_id", "res.users"),
];

const CHILD_FIELDS: &[FieldDecl] = &[
    FieldDecl::many2one("obj_id", OBJ)
        .required(true)
        .ondelete(OnDelete::Cascade),
    FieldDecl::char("label"),
    FieldDecl::integer("size"),
];

/// The companies of users 2..=5 (all of them; the principal's allowed companies are a subset).
pub fn user_companies(uid: i64) -> &'static [i64] {
    match uid {
        2 => &[1, 2, 3],
        3 => &[1],
        4 => &[2, 3],
        5 => &[1, 2],
        other => panic!("the fixture has no user {other}"),
    }
}

/// `base` (the policy tables), `ir.model.data` and the fixture models, on a fresh database,
/// deciding with `provider`; companies 1..=3 and users 2..=5 committed.
pub async fn harness(provider: Arc<dyn PolicyProvider>) -> Harness {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON, &ADDON])
        .with_decls([
            erp_data::ir_model_data_decl(),
            ModelDecl::new("x_props", PARTNER)
                .description("Props Partner")
                .fields(PARTNER_FIELDS),
            ModelDecl::extension("x_props", "res.users", &["res.users"]).fields(USER_FIELDS),
            ModelDecl::new("x_props", CATEG)
                .company_access(CompanyAccessDecl::unrestricted(
                    "security property fixture tests handwritten rules",
                ))
                .description("Props Category")
                .fields(CATEG_FIELDS),
            ModelDecl::new("x_props", OBJ)
                .company_access(CompanyAccessDecl::unrestricted(
                    "security property fixture tests handwritten rules",
                ))
                .description("Props Object")
                .fields(OBJ_FIELDS),
            ModelDecl::new("x_props", CHILD)
                .company_access(CompanyAccessDecl::unrestricted(
                    "security property fixture tests delegated access",
                ))
                .description("Props Child")
                .inherits(&[(OBJ, "obj_id")])
                .fields(CHILD_FIELDS),
        ])
        .policy(provider)
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    tx.execute(
        "INSERT INTO res_company (id, name, currency_id) VALUES \
         (1, 'c1', $1), (2, 'c2', $1), (3, 'c3', $1)",
        &[&currency],
    )
    .await
    .unwrap();
    let mut sql = String::from(
        "SELECT setval(pg_get_serial_sequence('res_company', 'id'), 3); \
         INSERT INTO res_users (id, name, login, active, share, company_id) VALUES ",
    );
    let users: Vec<String> = (2..=5)
        .map(|uid| {
            format!(
                "({uid}, 'u{uid}', 'u{uid}', true, false, {})",
                user_companies(uid)[0]
            )
        })
        .collect();
    sql.push_str(&users.join(", "));
    sql.push_str(
        "; SELECT setval(pg_get_serial_sequence('res_users', 'id'), 5); \
         INSERT INTO res_company_users_rel (user_id, cid) VALUES ",
    );
    let memberships: Vec<String> = (2..=5)
        .flat_map(|uid| {
            user_companies(uid)
                .iter()
                .map(move |c| format!("({uid}, {c})"))
        })
        .collect();
    sql.push_str(&memberships.join(", "));
    tx.batch(&sql).await.unwrap();
    tx.commit().await.unwrap();
    h
}

/// A provider serving whatever snapshot the test installed last (one per generated case).
pub struct SwapPolicy(RwLock<Arc<dyn AccessPolicy>>);

impl SwapPolicy {
    /// A provider serving `initial`.
    pub fn new(initial: Arc<dyn AccessPolicy>) -> Arc<Self> {
        Arc::new(Self(RwLock::new(initial)))
    }

    /// Serve `policy` to the transactions that begin from now on.
    pub fn set(&self, policy: Arc<dyn AccessPolicy>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = policy;
    }
}

impl PolicyProvider for SwapPolicy {
    fn current(&self) -> Arc<dyn AccessPolicy> {
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The tests write no policy table through the ORM: a private rebuild is never asked.
    fn rebuild_private<'a>(
        &'a self,
        _tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<Arc<dyn AccessPolicy>>> {
        Box::pin(async move {
            Err(erp_fields::Error::programming(
                "the property tests' policy is compiled from generated rows; no transaction \
                 writes a policy table",
            ))
        })
    }
}

/// A harness whose policy the test swaps per case, and the swap handle.
pub async fn swap_harness() -> (Harness, Arc<SwapPolicy>) {
    // the harness builds its schema as `System`, which no policy decides for
    let swap = SwapPolicy::new(Arc::new(empty_snapshot()));
    let provider: Arc<dyn PolicyProvider> = swap.clone();
    let h = harness(provider).await;
    swap.set(Arc::new(compile(h.registry(), &[], 1)));
    (h, swap)
}

fn empty_snapshot() -> PolicySnapshot {
    // `base` as linked: its `#[access_domain]` overrides (linked into this binary) need their
    // models' declarations
    let registry = Arc::new(
        erp_registry::RegistryBuilder::new([&erp_addon_base::ADDON])
            .build()
            .unwrap(),
    );
    Compiler::new(registry)
        .compile(&PolicyRows::builder(0).build().unwrap())
        .unwrap()
}

fn gid(id: i64) -> GroupId {
    GroupId::new(id).unwrap()
}

fn xmlid(text: &str) -> QualifiedXmlId {
    QualifiedXmlId::parse(text).unwrap()
}

/// The fixture's groups.
pub fn group_rows() -> Vec<GroupRow> {
    vec![
        GroupRow::new(gid(USER), "Internal User")
            .unwrap()
            .with_xmlid(xmlid("base.group_user")),
        GroupRow::new(gid(G1), "Group 1").unwrap(),
        GroupRow::new(gid(G2), "Group 2").unwrap(),
        GroupRow::new(gid(G3), "Group 3").unwrap(),
        GroupRow::new(gid(EVERYONE), "Everyone")
            .unwrap()
            .with_xmlid(xmlid("base.group_everyone")),
        GroupRow::new(gid(PORTAL), "Portal")
            .unwrap()
            .with_xmlid(xmlid("base.group_portal"))
            .with_share(true),
        GroupRow::new(gid(NO_ONE), "Technical Features")
            .unwrap()
            .with_xmlid(xmlid("base.group_no_one")),
    ]
}

/// The fixture's implications: both user types imply everyone, group 3 implies group 1.
pub fn implications() -> Vec<Implication> {
    vec![
        Implication::new(gid(USER), gid(EVERYONE)),
        Implication::new(gid(PORTAL), gid(EVERYONE)),
        Implication::new(gid(G3), gid(G1)),
    ]
}

/// The `ir.access` row of `spec`, id `id`.
pub fn rule_row(id: i64, spec: &RuleSpec) -> RuleRow {
    let mut row = RuleRow::new(
        RuleId::new(id).unwrap(),
        &format!("rule {id}"),
        spec.on.model(),
        &spec.operation(),
    )
    .unwrap();
    if let Some(group) = spec.group_id() {
        row = row.with_group(gid(group));
    }
    if let Some(text) = spec.text() {
        row = row.with_domain(&text);
    }
    row
}

/// The snapshot of `rules` (ids 1..) over the fixture groups, `policy` generation `generation`.
pub fn compile(registry: &Arc<Registry>, rules: &[RuleSpec], generation: u64) -> PolicySnapshot {
    let rows = PolicyRows::builder(generation)
        .groups(group_rows())
        .implications(implications())
        .rules(rules.iter().zip(1..).map(|(spec, id)| rule_row(id, spec)))
        .build()
        .unwrap();
    Compiler::new(Arc::clone(registry)).compile(&rows).unwrap()
}

/// The user principal of `spec`, its groups closed under `policy`, resolved with all its
/// companies.
pub fn principal(policy: &dyn AccessPolicy, spec: &PrincipalSpec) -> Principal {
    principal_with(policy, spec.uid, &spec.direct_groups(), &spec.allowed())
}

/// A resolved user principal of `uid` in `direct` groups with `allowed` companies.
pub fn principal_with(
    policy: &dyn AccessPolicy,
    uid: i64,
    direct: &[i64],
    allowed: &[i64],
) -> Principal {
    let direct: Vec<GroupId> = direct.iter().map(|g| gid(*g)).collect();
    let groups: GroupSet = policy.groups(&direct).unwrap();
    let company = |c: &i64| CompanyId::new(*c).unwrap();
    PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(
            Resolution::in_id_order(
                groups,
                user_companies(uid).iter().map(company),
                NonZeroU64::MIN,
            )
            .unwrap(),
        )
        .companies(allowed.iter().map(company).collect())
        .build()
        .unwrap()
}

/// The binding the engine builds for `principal` under `policy` (no elevation block, the
/// principal's own resolution).
pub fn bind<'p>(policy: &dyn AccessPolicy, principal: &'p Principal) -> PrincipalBinding<'p> {
    PrincipalBinding::bind(policy, principal, Arc::clone(principal.resolution())).unwrap()
}

// ----- seeding -----------------------------------------------------------------------------------

const NAMES: [Option<&str>; 7] = [
    None,
    Some(""),
    Some("a"),
    Some("b"),
    Some("ab"),
    Some("A"),
    Some("é"),
];
const CODES: [Option<&str>; 3] = [None, Some("x"), Some("y")];
const INTS: [Option<i64>; 8] = [
    None,
    Some(-1),
    Some(0),
    Some(1),
    Some(2),
    Some(3),
    Some(5),
    Some(8),
];
const AMOUNTS: [Option<f64>; 6] = [
    None,
    Some(0.0),
    Some(0.5),
    Some(1.5),
    Some(2.25),
    Some(-1.0),
];
const FLAGS: [Option<bool>; 3] = [None, Some(true), Some(false)];
/// Days from the UTC date of the seeding transaction.
const DAYS: [Option<i64>; 6] = [None, Some(-2), Some(-1), Some(0), Some(1), Some(2)];
/// Seconds from the seeding transaction's start, whole seconds: never within a second of a later
/// transaction's "now".
const MOMENTS: [Option<i64>; 5] = [
    None,
    Some(-172_800),
    Some(-3_600),
    Some(3_600),
    Some(172_800),
];
const COMPANIES: [Option<i64>; 4] = [None, Some(1), Some(2), Some(3)];
const USERS: [Option<i64>; 6] = [None, Some(1), Some(2), Some(3), Some(4), Some(5)];
const CATEG_NAMES: [Option<&str>; 4] = [None, Some("c1"), Some("c2"), Some("internal")];
const SEQS: [Option<i64>; 6] = [None, Some(0), Some(1), Some(2), Some(3), Some(5)];
const LABELS: [Option<&str>; 3] = [None, Some("l1"), Some("l2")];
const SIZES: [Option<i64>; 5] = [None, Some(0), Some(1), Some(2), Some(3)];

fn pick<T: Copy>(table: &[T], index: u8) -> T {
    table[usize::from(index) % table.len()]
}

fn text(value: Option<&str>) -> String {
    value.map_or_else(
        || "NULL".to_owned(),
        |s| format!("'{}'", s.replace('\'', "''")),
    )
}

fn num<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "NULL".to_owned(), |v| v.to_string())
}

/// A reference by index: 0 is `NULL`, `k` the `(k - 1) % len + 1`-th row; `NULL` without rows.
fn reference(index: u8, len: usize) -> Option<i64> {
    if index == 0 || len == 0 {
        return None;
    }
    Some(i64::try_from((usize::from(index) - 1) % len + 1).unwrap())
}

/// The ids of the rows `seed` writes for `case` (1..=n per model).
#[derive(Debug, Clone)]
pub struct Ids {
    pub objs: Vec<i64>,
    pub children: Vec<i64>,
    pub categs: Vec<i64>,
}

impl Ids {
    /// The ids of `model`.
    pub fn of(&self, model: &str) -> &[i64] {
        match model {
            OBJ => &self.objs,
            CHILD => &self.children,
            CATEG => &self.categs,
            other => panic!("no rows are generated for {other}"),
        }
    }
}

fn ids(n: usize) -> Vec<i64> {
    (1..=i64::try_from(n).unwrap()).collect()
}

/// Replace the fixture rows with `case`'s (as `System`, committed).
pub async fn seed(h: &Harness, case: &Case) -> Ids {
    let mut sql = String::from(
        "DELETE FROM x_child; DELETE FROM x_obj; DELETE FROM x_categ; \
         UPDATE res_users SET partner_id = NULL; DELETE FROM x_partner;",
    );
    let partners = case.partners.len();
    if partners > 0 {
        let rows: Vec<String> = case
            .partners
            .iter()
            .zip(1..)
            .map(|((name, _), id)| format!("({id}, {})", text(pick(&NAMES, *name))))
            .collect();
        sql.push_str(&format!(
            " INSERT INTO x_partner (id, name) VALUES {};",
            rows.join(", ")
        ));
        for ((_, commercial), id) in case.partners.iter().zip(1..) {
            if let Some(target) = reference(*commercial, partners) {
                sql.push_str(&format!(
                    " UPDATE x_partner SET commercial_partner_id = {target} WHERE id = {id};"
                ));
            }
        }
    }
    for (uid, partner) in (2..=5).zip(&case.user_partners) {
        if let Some(target) = reference(*partner, partners) {
            sql.push_str(&format!(
                " UPDATE res_users SET partner_id = {target} WHERE id = {uid};"
            ));
        }
    }
    let categs = case.categs.len();
    if categs > 0 {
        let rows: Vec<String> = case
            .categs
            .iter()
            .zip(1..)
            .map(|((name, seq, company), id)| {
                format!(
                    "({id}, {}, {}, {})",
                    text(pick(&CATEG_NAMES, *name)),
                    num(pick(&SEQS, *seq)),
                    num(pick(&COMPANIES, *company))
                )
            })
            .collect();
        sql.push_str(&format!(
            " INSERT INTO x_categ (id, name, seq, company_id) VALUES {};",
            rows.join(", ")
        ));
    }
    let objs = case.objs.len();
    if objs > 0 {
        let rows: Vec<String> = case
            .objs
            .iter()
            .zip(1..)
            .map(|(row, id)| {
                let v = row.0;
                let day = pick(&DAYS, v[6]).map_or_else(
                    || "NULL".to_owned(),
                    |d| format!("((now() AT TIME ZONE 'UTC')::date + {d})"),
                );
                let moment = pick(&MOMENTS, v[7]).map_or_else(
                    || "NULL".to_owned(),
                    |s| {
                        format!(
                            "(date_trunc('second', now() AT TIME ZONE 'UTC') + \
                             interval '{s} seconds')"
                        )
                    },
                );
                format!(
                    "({id}, {}, {}, {}, {}, {}, {}, {day}, {moment}, {}, {}, {}, {})",
                    text(pick(&NAMES, v[0])),
                    text(pick(&CODES, v[1])),
                    num(pick(&INTS, v[2])),
                    num(pick(&INTS, v[3])),
                    num(pick(&AMOUNTS, v[4])),
                    num(pick(&FLAGS, v[5])),
                    num(reference(v[8], categs)),
                    num(reference(v[9], partners)),
                    num(pick(&COMPANIES, v[10])),
                    num(pick(&USERS, v[11])),
                )
            })
            .collect();
        sql.push_str(&format!(
            " INSERT INTO x_obj (id, name, code, val, qty, amount, flag, day, moment, categ_id, \
             partner_id, company_id, user_id) VALUES {};",
            rows.join(", ")
        ));
    }
    let mut children = 0;
    if objs > 0 && !case.children.is_empty() {
        let rows: Vec<String> = case
            .children
            .iter()
            .zip(1..)
            .map(|((obj, label, size), id)| {
                format!(
                    "({id}, {}, {}, {})",
                    usize::from(*obj) % objs + 1,
                    text(pick(&LABELS, *label)),
                    num(pick(&SIZES, *size))
                )
            })
            .collect();
        children = rows.len();
        sql.push_str(&format!(
            " INSERT INTO x_child (id, obj_id, label, size) VALUES {};",
            rows.join(", ")
        ));
    }
    let mut tx = h.begin().await.unwrap();
    tx.batch(&sql).await.unwrap();
    tx.commit().await.unwrap();
    Ids {
        objs: ids(objs),
        children: ids(children),
        categs: ids(categs),
    }
}

// ----- runner ------------------------------------------------------------------------------------

/// A runner of `cases` cases with a fixed seed (the same cases in every CI run), keeping failing
/// seeds outside the source tree. `seeds` names a read-only fixture under `tests/seeds/`;
/// both that fixture and generated failures under workspace `target/` are replayed first.
pub fn runner(cases: u32, seed: u64, seeds: &'static str) -> TestRunner {
    TestRunner::new(Config {
        cases,
        max_shrink_iters: 256,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: Some(Box::new(persistence::Seeds::for_fixture(seeds))),
        ..Config::default()
    })
}

/// A heavier runner (the `#[ignore]`d full-size runs): a random seed unless `PROPTEST_RNG_SEED`
/// is set, failing seeds kept as by [`runner`].
pub fn heavy_runner(cases: u32, seeds: &'static str) -> TestRunner {
    TestRunner::new(Config {
        cases,
        max_shrink_iters: 1_024,
        rng_algorithm: RngAlgorithm::ChaCha,
        failure_persistence: Some(Box::new(persistence::Seeds::for_fixture(seeds))),
        ..Config::default()
    })
}

/// A failed case with `message`.
pub fn fail(message: String) -> TestCaseError {
    TestCaseError::fail(message)
}

/// The multi-threaded runtime the cases run on.
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}
