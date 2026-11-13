//! Enforcement of the generated company restrictions (A1-D2/A1-D3; design,
//! §6.4, §6.5, §6.7, §11.3) over the fixture "company
//! access": drift of the generated rows against the declarations, the empty validated
//! active-company set, the origin of a generated rule in explanations, and the new state of a
//! record a write moves. Generated rows are inserted into the policy rows with `generated = true`.
//! Cases that need no database build the policy from rows only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::{Arc, PoisonError, RwLock};

use erp_fields::ErrorKind;
use erp_orm::{
    AccessPolicy, ActiveCompanySet, BeginOptions, BoxFuture, CompanyId, GroupId, Operation,
    PolicyProvider, Principal, PrincipalBinding, PrincipalBuilder, QualifiedXmlId, Resolution,
    RuleOrigin, Transaction, UserId,
};
use erp_registry::{Registry, RegistryBuilder};
use erp_sdk::{Cell, Domain, RawVals};
use erp_security::company_access::{GeneratedRestriction, GeneratedRow, generate};
use erp_security::rows::{GroupRow, Implication, PolicyRows, RuleId, RuleRow};
use erp_security::snapshot::{Compiler, PolicySnapshot};
use erp_test_models::test_company_access;
use erp_testkit::Harness;

const P: &str = "test_company_access.";
/// Companies: R root, B and S branches of R, F a foreign root.
const R: i64 = 1;
const B: i64 = 2;
const S: i64 = 3;
const F: i64 = 4;
const GROUP: i64 = 1;

/// What a test does to the generated row of one model.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Intact,
    Missing,
    Archived,
    Domain,
    Operation,
    Group,
    Duplicate,
    Renamed,
}

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
                "this test writes no policy table",
            ))
        })
    }
}

fn registry() -> Arc<Registry> {
    Arc::new(
        RegistryBuilder::new([&test_company_access::ADDON])
            .build()
            .unwrap(),
    )
}

/// The rows of a policy that grants `crud` on every generated model and carries the generated
/// restrictions, each altered as `alter` says for its model's short name.
fn rows(registry: &Registry, alter: &dyn Fn(&str) -> Row, stale: &[&str]) -> PolicyRows {
    let gid = |n| GroupId::new(n).unwrap();
    let xml = |t: &str| QualifiedXmlId::parse(t).unwrap();
    let group = |n, name: &str, x: &str| GroupRow::new(gid(n), name).unwrap().with_xmlid(xml(x));
    let mut builder = PolicyRows::builder(1)
        .group(group(GROUP, "Internal User", "base.group_user"))
        .group(group(2, "Everyone", "base.group_everyone"))
        .group(group(3, "Portal", "base.group_portal").with_share(true))
        .group(group(4, "Technical Features", "base.group_no_one"))
        .implication(Implication::new(gid(GROUP), gid(2)))
        .implication(Implication::new(gid(3), gid(2)));
    let mut id = 1;
    for g in generate(registry).unwrap() {
        builder = builder.rule(
            RuleRow::new(RuleId::new(id).unwrap(), "perm", g.model_name(), "crud")
                .unwrap()
                .with_group(GroupId::new(GROUP).unwrap()),
        );
        let short = g.model_name().trim_start_matches(P);
        let row_id = RuleId::new(id + 1).unwrap();
        let plain = |name: &str, domain: &str, op: &str| {
            RuleRow::new(row_id, name, g.model_name(), op)
                .unwrap()
                .with_domain(domain)
                .with_generated()
        };
        match alter(short) {
            Row::Intact => builder = builder.rule(plain(g.name(), g.text(), "crud")),
            Row::Missing => {}
            Row::Archived => {
                builder = builder.archived_generated(
                    GeneratedRow::new(row_id, g.model_name(), g.name(), false, "crud")
                        .with_domain(g.text()),
                );
            }
            Row::Domain => {
                builder = builder.rule(plain(g.name(), "[('id', '>', 0)]", "crud"));
            }
            Row::Operation => builder = builder.rule(plain(g.name(), g.text(), "r")),
            Row::Group => {
                builder = builder.rule(
                    plain(g.name(), g.text(), "crud").with_group(GroupId::new(GROUP).unwrap()),
                );
            }
            Row::Duplicate => {
                builder = builder.rule(plain(g.name(), g.text(), "crud")).rule(
                    RuleRow::new(
                        RuleId::new(id + 1000).unwrap(),
                        g.name(),
                        g.model_name(),
                        "crud",
                    )
                    .unwrap()
                    .with_domain(g.text())
                    .with_generated(),
                );
            }
            Row::Renamed => builder = builder.rule(plain("renamed", g.text(), "crud")),
        }
        id += 2;
    }
    // generated rows of models that generate nothing
    for model in stale {
        builder = builder.rule(
            RuleRow::new(RuleId::new(id).unwrap(), "stale", model, "crud")
                .unwrap()
                .with_domain("[('id', '>', 0)]")
                .with_generated(),
        );
        id += 1;
    }
    builder.build().unwrap()
}

fn policy(registry: &Arc<Registry>, alter: &dyn Fn(&str) -> Row) -> PolicySnapshot {
    Compiler::new(Arc::clone(registry))
        .compile(&rows(registry, alter, &[]))
        .unwrap()
}

fn intact(_: &str) -> Row {
    Row::Intact
}

fn principal(policy: &dyn AccessPolicy, active: &[i64], companies: &[i64]) -> Principal {
    let groups = policy.groups(&[GroupId::new(GROUP).unwrap()]).unwrap();
    let company = |c: &i64| CompanyId::new(*c).unwrap();
    PrincipalBuilder::user(UserId::new(2).unwrap())
        .resolution(
            Resolution::in_id_order(groups, companies.iter().map(company), NonZeroU64::MIN)
                .unwrap(),
        )
        .companies(active.iter().map(company).collect())
        .build()
        .unwrap()
}

fn user(policy: &dyn AccessPolicy, active: &[i64]) -> Principal {
    principal(policy, active, &[R, B, S, F])
}

fn bind<'p>(
    policy: &dyn AccessPolicy,
    principal: &'p Principal,
    active: ActiveCompanySet,
) -> PrincipalBinding<'p> {
    PrincipalBinding::bind(policy, principal, Arc::clone(principal.resolution()))
        .unwrap()
        .with_companies(active)
}

fn generated<'a>(all: &'a [GeneratedRestriction], short: &str) -> &'a GeneratedRestriction {
    all.iter()
        .find(|g| g.model_name() == format!("{P}{short}"))
        .unwrap()
}

fn drift_text(g: &GeneratedRestriction, problem: &str) -> String {
    format!(
        "The generated company restriction of {m} ({s}, declared by addon '{a}') {problem}; \
         access to {m} is refused until an update of addon '{a}' restores it.",
        m = g.model_name(),
        s = g.spelling(),
        a = g.source().addon()
    )
}

/// rows deny their model for every operation with the full text, renamed and orphan rows are
/// reported and deny nothing; the `erp access report` text is the build report's)
#[test]
fn drifted_generated_rows_deny_their_model() {
    let reg = registry();
    let all = generate(&reg).unwrap();
    let cases = [
        ("m1", Row::Missing, "is missing"),
        ("m2", Row::Archived, "is archived"),
        (
            "m3",
            Row::Domain,
            "was changed (domain); does not name the company",
        ),
        ("m4", Row::Operation, "was changed (operation)"),
        ("m5", Row::Group, "was changed (group)"),
        ("m6", Row::Duplicate, "exists 2 times"),
    ];
    let alter = |short: &str| {
        cases
            .iter()
            .find(|(s, _, _)| *s == short)
            .map_or(Row::Intact, |(_, row, _)| *row)
    };
    let snapshot = policy(&reg, &alter);
    let p = user(&snapshot, &[B]);
    let b = bind(&snapshot, &p, ActiveCompanySet::NonEmpty);
    for (short, _, problem) in cases {
        let spec = reg.model(&format!("{P}{short}")).unwrap();
        let want = drift_text(generated(&all, short), problem);
        for op in [
            Operation::Read,
            Operation::Write,
            Operation::Create,
            Operation::Unlink,
        ] {
            let err = snapshot.check_model(&b, spec, op).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Access, "{short} {op}");
            assert_eq!(err.to_string(), want, "{short} {op}");
            let err = snapshot.access_domain(&b, spec, op).unwrap_err();
            assert_eq!(err.to_string(), want, "{short} {op} domain");
        }
    }
    // a model with an intact row still decides
    let m7 = reg.model(&format!("{P}m7")).unwrap();
    snapshot.check_model(&b, m7, Operation::Read).unwrap();
    // the build report lists every problem; each denies its model
    let report = snapshot.report();
    assert!(report.has_errors());
    let text = report.to_string();
    for (short, _, problem) in cases {
        let g = generated(&all, short);
        // one line per finding: the message joins them with "; "
        for part in problem.split("; ") {
            let line = format!(
                "error: generated company restriction of {m} ({s}, addon '{a}') {part} \
                 [denies {m}]",
                m = g.model_name(),
                s = g.spelling(),
                a = g.source().addon()
            );
            assert!(text.contains(&line), "missing report line: {line}\n{text}");
        }
    }
    assert_eq!(report.errors_by_model().len(), cases.len());
}

/// A renamed row is reported but the model is not denied; an orphan generated row (its model
/// generates nothing) only narrows and is reported.
#[test]
fn renamed_and_orphan_rows_are_reported_and_deny_nothing() {
    let reg = registry();
    let renamed = |short: &str| {
        if short == "m1" {
            Row::Renamed
        } else {
            Row::Intact
        }
    };
    let snapshot = policy(&reg, &renamed);
    let p = user(&snapshot, &[B]);
    let b = bind(&snapshot, &p, ActiveCompanySet::NonEmpty);
    let m1 = reg.model(&format!("{P}m1")).unwrap();
    snapshot.check_model(&b, m1, Operation::Read).unwrap();
    let issues = snapshot.report().generated_issues();
    assert_eq!(issues.len(), 1);
    assert!(!issues[0].denies());
    assert!(issues[0].to_string().contains("was renamed"));
    assert!(!issues[0].to_string().contains("[denies"));

    // an orphan: a generated row on a model whose declaration is `unrestricted`
    let stale = format!("{P}u");
    let snapshot = Compiler::new(Arc::clone(&reg))
        .compile(&rows(&reg, &intact, &[stale.as_str()]))
        .unwrap();
    let issues = snapshot.report().generated_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(!issues[0].denies());
    assert!(issues[0].to_string().contains("no longer generates"));
}

/// shown on the predicates; the rows it admits and hides are the PostgreSQL case below)
#[test]
fn principals_without_companies_compile_apart() {
    let reg = registry();
    let snapshot = policy(&reg, &intact);
    let all = generate(&reg).unwrap();
    let p = user(&snapshot, &[B]);
    let full = bind(&snapshot, &p, ActiveCompanySet::NonEmpty);
    let empty = bind(&snapshot, &p, ActiveCompanySet::Empty);
    assert_eq!(full.shape().companies(), ActiveCompanySet::NonEmpty);
    assert_eq!(empty.shape().companies(), ActiveCompanySet::Empty);
    assert_ne!(full.shape(), empty.shape(), "the set is part of the shape");
    for g in &all {
        let spec = reg.model(g.model_name()).unwrap();
        let a = snapshot
            .access_domain(&full, spec, Operation::Read)
            .unwrap();
        let b = snapshot
            .access_domain(&empty, spec, Operation::Read)
            .unwrap();
        // the empty variant is never a constant: the model-level check passes
        assert_eq!(b.constant(), None, "{}", g.model_name());
        if g.text() == g.deny_text() {
            assert_eq!(
                a.domain(),
                b.domain(),
                "{}: no allow to replace",
                g.model_name()
            );
        } else {
            assert_ne!(
                a.domain(),
                b.domain(),
                "{}: the allow is replaced",
                g.model_name()
            );
        }
        snapshot.check_model(&empty, spec, Operation::Read).unwrap();
        // the variants never share a memo entry: asking again gives each its own answer
        assert_eq!(
            snapshot
                .access_domain(&full, spec, Operation::Read)
                .unwrap()
                .domain(),
            a.domain()
        );
        assert_eq!(
            snapshot
                .access_domain(&empty, spec, Operation::Read)
                .unwrap()
                .domain(),
            b.domain()
        );
    }
}

/// in the explanation columns; an ordinary rule is `Data`)
#[test]
fn explain_names_the_generated_restriction_and_its_declaration() {
    let reg = registry();
    let snapshot = policy(&reg, &intact);
    let all = generate(&reg).unwrap();
    let p = user(&snapshot, &[B]);
    let b = bind(&snapshot, &p, ActiveCompanySet::NonEmpty);
    let spec = reg.model(&format!("{P}m1")).unwrap();
    let want = generated(&all, "m1");
    let request = erp_orm::ExplainRequest::new(&b, spec, Operation::Read)
        .ids(&[1])
        .unwrap();
    let answer = snapshot.explain(&request).unwrap();
    let origins: Vec<&RuleOrigin> = answer.columns().iter().map(|c| c.rule().origin()).collect();
    assert!(
        origins.iter().any(|o| matches!(
            o,
            RuleOrigin::Generated { model, addon, file, line }
                if &**model == want.model_name()
                    && &**addon == want.source().addon()
                    && &**file == want.source().file()
                    && *line == want.source().line()
        )),
        "{origins:?}"
    );
    let named: Vec<&str> = answer.columns().iter().map(|c| c.rule().name()).collect();
    assert!(named.contains(&want.name()), "{named:?}");
    let perm: Vec<_> = answer
        .columns()
        .iter()
        .filter(|c| c.rule().name() == "perm")
        .collect();
    assert!(!perm.is_empty(), "the permission is a column");
    assert!(perm.iter().all(|c| *c.rule().origin() == RuleOrigin::Data));
}

async fn harness() -> (Harness, Arc<dyn AccessPolicy>) {
    let pre = registry();
    let open: Arc<dyn AccessPolicy> = Arc::new(
        Compiler::new(pre)
            .compile(&PolicyRows::builder(0).build().unwrap())
            .unwrap(),
    );
    let swap = Arc::new(Swap(RwLock::new(Arc::clone(&open))));
    let provider: Arc<dyn PolicyProvider> = swap.clone();
    let h = Harness::builder([&test_company_access::ADDON])
        .policy(provider)
        .build()
        .await
        .expect("harness");
    let policy: Arc<dyn AccessPolicy> = Arc::new(
        Compiler::new(Arc::clone(h.registry()))
            .compile(&rows(h.registry(), &intact, &[]))
            .unwrap(),
    );
    *swap.0.write().unwrap() = Arc::clone(&policy);
    (h, policy)
}

async fn seed_companies(tx: &mut Transaction) {
    tx.batch(
        "INSERT INTO res_company (id, name, parent_id, parent_path) VALUES \
         (1, 'R', NULL, '1/'), (2, 'B', 1, '1/2/'), (3, 'S', 1, '1/3/'), (4, 'F', NULL, '4/'); \
         SELECT setval(pg_get_serial_sequence('res_company', 'id'), 4); \
         INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true)",
    )
    .await
    .unwrap();
}

async fn insert(tx: &mut Transaction, table: &str, cols: &str, vals: &str) -> i64 {
    let sql = if cols.is_empty() {
        format!("INSERT INTO {table} (name) VALUES ('r') RETURNING id")
    } else {
        format!("INSERT INTO {table} (name, {cols}) VALUES ('r', {vals}) RETURNING id")
    };
    tx.query(&sql, &[]).await.unwrap()[0].get(0)
}

async fn visible(tx: &mut Transaction, model: &str) -> BTreeSet<i64> {
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

fn id_vals(field: &'static str, id: i64) -> RawVals {
    RawVals::new().set_cell(field, Cell::Id(id))
}

/// `generated_restrictions_apply_to_search_read_and_create`: the engine takes the generated rule
/// like any restriction: a search hides foreign rows, a read by id fails with the record-level
/// error and the multi-company hint, a create in a foreign company fails and one in an active
/// company works.
#[tokio::test]
async fn generated_restrictions_apply_to_search_read_and_create() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    let own = insert(
        &mut tx,
        "test_company_access_m1",
        "company_id",
        &B.to_string(),
    )
    .await;
    let foreign = insert(
        &mut tx,
        "test_company_access_m1",
        "company_id",
        &F.to_string(),
    )
    .await;
    tx.commit().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(user(&*policy, &[B])))
        .await
        .unwrap();
    let model = format!("{P}m1");
    assert_eq!(visible(&mut tx, &model).await, BTreeSet::from([own]));
    let records = tx.env().records(&model).unwrap();
    let err = records
        .browse([foreign])
        .unwrap()
        .read(&mut tx, &["name"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert!(
        err.to_string().contains("multi-company issue"),
        "company hint: {err}"
    );
    assert!(err.context().is_some(), "the suggested company rides along");
    let err = records
        .create(&mut tx, vec![id_vals("company_id", F)])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    records
        .create(&mut tx, vec![id_vals("company_id", B)])
        .await
        .unwrap();
}

/// whose validated active-company set is empty, the allow forms included; the 17 account models
/// come with)
#[tokio::test]
async fn an_empty_active_company_set_admits_nothing() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    // one record of company B and one without company, for every form
    let reg = Arc::clone(h.registry());
    let mut seeded = Vec::new();
    for short in ["m1", "m2", "m3", "m4"] {
        let table = &reg.model(&format!("{P}{short}")).unwrap().table;
        let with = insert(&mut tx, table, "company_id", &B.to_string()).await;
        let without = insert(&mut tx, table, "company_id", "NULL").await;
        seeded.push((format!("{P}{short}"), with, without));
    }
    for short in ["m5", "m6", "m7", "m8"] {
        let spec = reg.model(&format!("{P}{short}")).unwrap();
        let with = insert(&mut tx, &spec.table, "", "").await;
        let without = insert(&mut tx, &spec.table, "", "").await;
        let erp_fields::FieldType::Many2many {
            relation: Some(rel),
            ..
        } = &spec.field("company_ids").unwrap().ty
        else {
            panic!("company_ids is not a stored many2many");
        };
        tx.execute(
            &format!(
                "INSERT INTO {} ({}, {}) VALUES ({with}, {B})",
                rel.table, rel.column1, rel.column2
            ),
            &[],
        )
        .await
        .unwrap();
        seeded.push((format!("{P}{short}"), with, without));
    }
    tx.commit().await.unwrap();

    // control: with company B active, the allow forms admit the record without company
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(user(&*policy, &[B])))
        .await
        .unwrap();
    for (model, with, without) in &seeded {
        let found = visible(&mut tx, model).await;
        assert!(found.contains(with), "{model}: own record visible");
        let allows = matches!(model.trim_start_matches(P), "m3" | "m4" | "m7" | "m8");
        assert_eq!(found.contains(without), allows, "{model}: company-less");
    }
    // a subject with no company at all: nothing is admitted, the allow forms included
    let none = principal(&*policy, &[], &[]);
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(none))
        .await
        .unwrap();
    for (model, with, _) in &seeded {
        assert!(
            visible(&mut tx, model).await.is_empty(),
            "{model}: a search admits nothing"
        );
        let err = tx
            .env()
            .records(model)
            .unwrap()
            .browse([*with])
            .unwrap()
            .read(&mut tx, &["name"])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{model}: {err}");
    }
}

/// the fixture models; `Full` is not checked; the upsert update branch and the one2many link
/// share the hook and have no fixture model with a conflict target or an owner collection)
#[tokio::test]
async fn moving_a_record_checks_its_new_state() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    let m1 = insert(
        &mut tx,
        "test_company_access_m1",
        "company_id",
        &B.to_string(),
    )
    .await;
    let doc_s = insert(
        &mut tx,
        "test_company_access_m1",
        "company_id",
        &S.to_string(),
    )
    .await;
    let doc_f = insert(
        &mut tx,
        "test_company_access_m1",
        "company_id",
        &F.to_string(),
    )
    .await;
    // a line follows its document: the generated owner restriction (`l` has no company of its own,
    // so company consistency has nothing to say about where its document is)
    let line = insert(&mut tx, "test_company_access_l", "doc_id", &m1.to_string()).await;
    tx.commit().await.unwrap();

    // a failed write leaves its values in the transaction (as any failed check does): each refused
    // move runs in a transaction of its own, rolled back
    let begin = |active: &[i64]| {
        let p = user(&*policy, active);
        h.orm().begin_opts(BeginOptions::for_principal(p))
    };
    let recs = |tx: &Transaction, short: &str, id: i64| {
        tx.env()
            .records(&format!("{P}{short}"))
            .unwrap()
            .browse([id])
            .unwrap()
    };
    // active [B]: S and F are outside
    for (short, id, field, to) in [
        ("m1", m1, "company_id", F),
        ("m1", m1, "company_id", S),
        ("l", line, "doc_id", doc_f),
        ("l", line, "doc_id", doc_s),
    ] {
        let mut tx = begin(&[B]).await.unwrap();
        let err = recs(&tx, short, id)
            .write(&mut tx, id_vals(field, to))
            .await
            .unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::Access,
            "{short}.{field} -> {to}: {err}"
        );
        assert!(
            err.to_string().contains("doesn't have 'write' access"),
            "{short}.{field} -> {to}: {err}"
        );
        tx.rollback().await.unwrap();
    }
    let mut tx = begin(&[B]).await.unwrap();
    // a write of a field that is no anchor is not checked
    recs(&tx, "m1", m1)
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("renamed".into())),
        )
        .await
        .unwrap();
    // a move within the active companies passes
    recs(&tx, "m1", m1)
        .write(&mut tx, id_vals("company_id", B))
        .await
        .unwrap();
    // superuser mode is not checked
    recs(&tx, "m1", m1)
        .sudo(true)
        .write(&mut tx, id_vals("company_id", F))
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    // with B and S active the same move to S passes
    let mut tx = begin(&[B, S]).await.unwrap();
    recs(&tx, "m1", m1)
        .write(&mut tx, id_vals("company_id", S))
        .await
        .unwrap();
    recs(&tx, "l", line)
        .write(&mut tx, id_vals("doc_id", doc_s))
        .await
        .unwrap();
}

/// fields, none of them written, is refused; fixture model, `Full` is not checked)
#[tokio::test]
async fn a_related_company_chain_moves_with_its_anchor() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    let in_b = insert(
        &mut tx,
        "test_company_access_m1",
        "company_id",
        &B.to_string(),
    )
    .await;
    let in_f = insert(
        &mut tx,
        "test_company_access_m1",
        "company_id",
        &F.to_string(),
    )
    .await;
    let in_b2 = insert(
        &mut tx,
        "test_company_access_m1",
        "company_id",
        &B.to_string(),
    )
    .await;
    let rc = insert(
        &mut tx,
        "test_company_access_rc",
        "anchor_id, mid_company_id, company_id",
        &format!("{in_b}, {B}, {B}"),
    )
    .await;
    tx.commit().await.unwrap();

    let begin = || {
        let p = user(&*policy, &[B]);
        h.orm().begin_opts(BeginOptions::for_principal(p))
    };
    let recs = |tx: &Transaction| {
        tx.env()
            .records(&format!("{P}rc"))
            .unwrap()
            .browse([rc])
            .unwrap()
    };
    // the anchor moves to a company outside the active set: company_id follows it
    let mut tx = begin().await.unwrap();
    let err = recs(&tx)
        .write(&mut tx, id_vals("anchor_id", in_f))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert!(
        err.to_string().contains("doesn't have 'write' access"),
        "{err}"
    );
    tx.rollback().await.unwrap();
    // a move to another record of an active company passes
    let mut tx = begin().await.unwrap();
    recs(&tx)
        .write(&mut tx, id_vals("anchor_id", in_b2))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

fn cid(c: i64) -> CompanyId {
    CompanyId::new(c).unwrap()
}

/// Seed one `m1` record in each of B, S and F; their ids in that order.
async fn seed_m1(h: &Harness) -> (i64, i64, i64) {
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    let mut ids = Vec::new();
    for company in [B, S, F] {
        ids.push(
            insert(
                &mut tx,
                "test_company_access_m1",
                "company_id",
                &company.to_string(),
            )
            .await,
        );
    }
    tx.commit().await.unwrap();
    (ids[0], ids[1], ids[2])
}

/// restriction admits under an explicit active set [B] and [B, F], for a principal whose own
/// companies differ from the set; input order kept, duplicates and missing ids dropped)
#[tokio::test]
async fn visible_under_company_restriction_follows_the_given_companies() {
    let (h, policy) = harness().await;
    let (in_b, in_s, in_f) = seed_m1(&h).await;
    let model = format!("{P}m1");
    // the principal names [B, F]; the set passed in decides, never the principal's own list
    let p = user(&*policy, &[B, F]);
    let mut tx = h.begin().await.unwrap();
    let all = [in_f, in_s, in_b, in_f, i64::from(i32::MAX)];
    let only_b = tx
        .visible_under_company_restriction(&model, &all, &*policy, &p, &[cid(B)])
        .await
        .unwrap();
    assert_eq!(only_b, vec![in_b], "a record of F is hidden under [B]");
    let both = tx
        .visible_under_company_restriction(&model, &all, &*policy, &p, &[cid(B), cid(F)])
        .await
        .unwrap();
    assert_eq!(both, vec![in_f, in_b], "input order, each id once");
    tx.rollback().await.unwrap();
}

/// The visible set equals the write path's verdict on the same rows: a write that keeps each
/// record in its own company passes exactly for the records the call answers.
#[tokio::test]
async fn visible_under_company_restriction_matches_the_write_verdict() {
    let (h, policy) = harness().await;
    let (in_b, in_s, in_f) = seed_m1(&h).await;
    let model = format!("{P}m1");
    for active in [vec![B], vec![B, F], vec![S]] {
        let set: Vec<CompanyId> = active.iter().copied().map(cid).collect();
        let mut tx = h.begin().await.unwrap();
        let p = user(&*policy, &[R, B, S, F]);
        let visible = tx
            .visible_under_company_restriction(&model, &[in_b, in_s, in_f], &*policy, &p, &set)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let mut written = Vec::new();
        for (id, company) in [(in_b, B), (in_s, S), (in_f, F)] {
            let mut tx = h
                .orm()
                .begin_opts(BeginOptions::for_principal(user(&*policy, &active)))
                .await
                .unwrap();
            let outcome = tx
                .env()
                .records(&model)
                .unwrap()
                .browse([id])
                .unwrap()
                .write(&mut tx, id_vals("company_id", company))
                .await;
            if outcome.is_ok() {
                written.push(id);
            }
            tx.rollback().await.unwrap();
        }
        assert_eq!(visible, written, "active {active:?}");
    }
}

#[tokio::test]
async fn visible_under_company_restriction_with_no_company_shows_nothing() {
    let (h, policy) = harness().await;
    let (in_b, in_s, in_f) = seed_m1(&h).await;
    let p = user(&*policy, &[B]);
    let mut tx = h.begin().await.unwrap();
    let visible = tx
        .visible_under_company_restriction(
            &format!("{P}m1"),
            &[in_b, in_s, in_f],
            &*policy,
            &p,
            &[],
        )
        .await
        .unwrap();
    assert!(visible.is_empty(), "{visible:?}");
}

/// A company outside the principal's own is refused as an unauthorized company set.
#[tokio::test]
async fn visible_under_company_restriction_refuses_a_foreign_company() {
    let (h, policy) = harness().await;
    let (in_b, _, _) = seed_m1(&h).await;
    let p = principal(&*policy, &[B], &[R, B]);
    let want: erp_fields::Error = p.with_companies(vec![cid(F)]).unwrap_err().into();
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .visible_under_company_restriction(&format!("{P}m1"), &[in_b], &*policy, &p, &[cid(F)])
        .await
        .unwrap_err();
    assert_eq!((err.kind(), err.message()), (want.kind(), want.message()));
    assert_eq!(err.kind(), ErrorKind::Access);
}

/// Programming errors: an elevated or `System` principal, an unknown model, a model without a
/// generated restriction, a non-positive id, and a policy that answers no restriction for a
/// model that declares one.
#[tokio::test]
async fn visible_under_company_restriction_refuses_what_it_cannot_decide() {
    let (h, policy) = harness().await;
    let (in_b, _, _) = seed_m1(&h).await;
    let m1 = format!("{P}m1");
    let p = user(&*policy, &[B]);
    let b = [cid(B)];
    let mut tx = h.begin().await.unwrap();
    let system = tx.env().principal().clone();
    let err = tx
        .visible_under_company_restriction(&m1, &[in_b], &*policy, &system, &b)
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "visible_under_company_restriction decides for a human principal: the system \
             principal of user 1 is elevated or the system"
        )
    );
    let err = tx
        .visible_under_company_restriction("test_company_access.nope", &[in_b], &*policy, &p, &b)
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "visible_under_company_restriction: unknown model 'test_company_access.nope'"
        )
    );
    let err = tx
        .visible_under_company_restriction(&format!("{P}u"), &[in_b], &*policy, &p, &b)
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "visible_under_company_restriction: model 'test_company_access.u' declares no \
             generated company restriction"
        )
    );
    let err = tx
        .visible_under_company_restriction(&m1, &[in_b, 0], &*policy, &p, &b)
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "visible_under_company_restriction: invalid record id 0"
        )
    );
    tx.rollback().await.unwrap();

    // an elevated environment of the same user
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(user(&*policy, &[B])))
        .await
        .unwrap();
    let elevated = tx.env().sudo(true).principal().clone();
    let err = tx
        .visible_under_company_restriction(&m1, &[in_b], &*policy, &elevated, &b)
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "visible_under_company_restriction decides for a human principal: the user \
             principal of user 2 is elevated or the system"
        )
    );
    tx.rollback().await.unwrap();

    // a policy without generated restrictions (the static test policy) answers `None`
    let st = Harness::builder([&test_company_access::ADDON])
        .policy(erp_orm::StaticPolicy::AllowAll)
        .build()
        .await
        .expect("harness");
    let mut tx = st.begin().await.unwrap();
    let static_policy = Arc::clone(tx.policy());
    let err = tx
        .visible_under_company_restriction(&m1, &[1], &*static_policy, &p, &b)
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "the access policy answers no generated company restriction for \
             'test_company_access.m1', which declares one"
        )
    );
}
