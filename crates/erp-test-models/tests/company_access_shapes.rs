//! The generated company restrictions admit the declared rows on PostgreSQL (A1-D2;
//! design, §11.2): the eight direct forms over own, ancestor,
//! sibling, foreign and company-less records, for active sets `[B]` and `[B, F]`. Expected values
//! come from an independent statement of the admission rule below (`admits`), not from the
//! generated text. Rows are compiled as ordinary restrictions through the real policy compiler.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::{Arc, PoisonError, RwLock};

use erp_fields::FieldType;
use erp_orm::{
    AccessPolicy, BeginOptions, BoxFuture, CompanyId, GroupId, PolicyProvider, Principal,
    PrincipalBuilder, QualifiedXmlId, ReadGroup, Resolution, Transaction, UserId,
};
use erp_registry::{ModelSpec, Registry, RegistryBuilder};
use erp_sdk::Domain;
use erp_security::company_access::generate;
use erp_security::rows::{GroupRow, Implication, PolicyRows, RuleId, RuleRow};
use erp_security::snapshot::Compiler;
use erp_test_models::test_company_access;
use erp_testkit::Harness;

const P: &str = "test_company_access.";
/// Companies: R root, B and S branches of R, F a foreign root.
const R: i64 = 1;
const B: i64 = 2;
const S: i64 = 3;
const F: i64 = 4;
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

fn rows(registry: &Registry, restrictions: bool) -> PolicyRows {
    // the groups the policy compiler needs to tell user types apart (`base.group_*`)
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
        // a permission of the group for every operation, then (optionally) the restriction
        builder = builder.rule(
            RuleRow::new(RuleId::new(id).unwrap(), "perm", g.model_name(), "crud")
                .unwrap()
                .with_group(GroupId::new(GROUP).unwrap()),
        );
        if restrictions {
            builder = builder.rule(
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
        }
        id += 2;
    }
    builder.build().unwrap()
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
            .compile(&rows(h.registry(), true))
            .unwrap(),
    );
    *swap.0.write().unwrap() = Arc::clone(&policy);
    (h, policy)
}

fn principal(policy: &dyn AccessPolicy, active: &[i64]) -> Principal {
    let groups = policy.groups(&[GroupId::new(GROUP).unwrap()]).unwrap();
    let company = |c: &i64| CompanyId::new(*c).unwrap();
    PrincipalBuilder::user(UserId::new(2).unwrap())
        .resolution(
            Resolution::in_id_order(groups, [R, B, S, F].iter().map(company), NonZeroU64::MIN)
                .unwrap(),
        )
        .companies(active.iter().map(company).collect())
        .build()
        .unwrap()
}

/// The company ids a record of company `c` is visible through when `c` is an ancestor-or-self
/// of an active company (`parent_of`): R is the parent of B and S.
fn ancestors_or_self(c: i64) -> Vec<i64> {
    match c {
        B | S => vec![c, R],
        other => vec![other],
    }
}

/// The admission rule of design §4.1, stated on sets: does a record with `companies` (empty =
/// no company) pass the declared form for the active set `active`?
fn admits(many: bool, ancestor: bool, allow: bool, companies: &[i64], active: &[i64]) -> bool {
    let _ = many;
    if companies.is_empty() {
        return allow;
    }
    let reach: BTreeSet<i64> = if ancestor {
        active.iter().flat_map(|c| ancestors_or_self(*c)).collect()
    } else {
        active.iter().copied().collect()
    };
    companies.iter().any(|c| reach.contains(c))
}

async fn seed_companies(tx: &mut Transaction) {
    tx.batch(
        "INSERT INTO res_company (id, name, parent_id, parent_path) VALUES \
         (1, 'R', NULL, '1/'), (2, 'B', 1, '1/2/'), (3, 'S', 1, '1/3/'), (4, 'F', NULL, '4/'); \
         SELECT setval(pg_get_serial_sequence('res_company', 'id'), 4)",
    )
    .await
    .unwrap();
}

/// Insert one record per company set; return `(id, companies)`.
async fn seed_records(
    tx: &mut Transaction,
    spec: &ModelSpec,
    sets: &[Vec<i64>],
) -> Vec<(i64, Vec<i64>)> {
    let many = spec.field("company_ids").is_some();
    let mut out = Vec::new();
    for companies in sets {
        let sql = if many {
            format!(
                "INSERT INTO {} (name) VALUES ('r') RETURNING id",
                spec.table
            )
        } else {
            let value = companies
                .first()
                .map_or("NULL".to_owned(), ToString::to_string);
            format!(
                "INSERT INTO {} (name, company_id) VALUES ('r', {value}) RETURNING id",
                spec.table
            )
        };
        let id: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);
        if many {
            let FieldType::Many2many {
                relation: Some(rel),
                ..
            } = &spec.field("company_ids").unwrap().ty
            else {
                panic!("company_ids is not a stored many2many");
            };
            for c in companies {
                tx.execute(
                    &format!(
                        "INSERT INTO {} ({}, {}) VALUES ({id}, {c})",
                        rel.table, rel.column1, rel.column2
                    ),
                    &[],
                )
                .await
                .unwrap();
            }
        }
        out.push((id, companies.clone()));
    }
    out
}

/// is the proposed id of this case.
#[tokio::test]
async fn direct_forms_admit_the_declared_rows() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    let forms: [(&str, bool, bool, bool); 8] = [
        ("m1", false, false, false),
        ("m2", false, true, false),
        ("m3", false, true, true),
        ("m4", false, false, true),
        ("m5", true, true, false),
        ("m6", true, false, false),
        ("m7", true, false, true),
        ("m8", true, true, true),
    ];
    let single_sets: Vec<Vec<i64>> = vec![vec![B], vec![R], vec![S], vec![F], vec![]];
    let many_sets: Vec<Vec<i64>> = vec![vec![B], vec![R], vec![S], vec![F], vec![], vec![B, F]];
    let mut seeded = Vec::new();
    for (short, many, ancestor, allow) in forms {
        let spec = h.registry().model(&format!("{P}{short}")).unwrap();
        let sets = if many { &many_sets } else { &single_sets };
        seeded.push((
            spec.name,
            many,
            ancestor,
            allow,
            seed_records(&mut tx, spec, sets).await,
        ));
    }
    tx.commit().await.unwrap();

    let (mut compared, mut admitted, mut hidden) = (0, 0, 0);
    for active in [vec![B], vec![B, F]] {
        let mut tx = h
            .orm()
            .begin_opts(BeginOptions::for_principal(principal(&*policy, &active)))
            .await
            .unwrap();
        for (model, many, ancestor, allow, records) in &seeded {
            let found: BTreeSet<i64> = tx
                .env()
                .records(model)
                .unwrap()
                .search(&mut tx, &Domain::TRUE)
                .await
                .unwrap()
                .ids()
                .into_iter()
                .collect();
            let want: BTreeSet<i64> = records
                .iter()
                .filter(|(_, c)| admits(*many, *ancestor, *allow, c, &active))
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(found, want, "{model} for active companies {active:?}");
            compared += 1;
            admitted += want.len();
            hidden += records.len() - want.len();
        }
    }
    assert_eq!(compared, 16);
    // not vacuous: the forms admit some rows and hide others
    assert!(
        admitted > 0 && hidden > 0,
        "admitted {admitted}, hidden {hidden}"
    );
}

/// One row of `spec` with the given foreign keys: `INSERT INTO t (name, <cols>) VALUES ('r', <ids>)`.
async fn insert_child(
    tx: &mut Transaction,
    spec: &ModelSpec,
    columns: &[(&str, Option<i64>)],
) -> i64 {
    let names: Vec<_> = columns.iter().map(|(n, _)| *n).collect();
    let values: Vec<_> = columns
        .iter()
        .map(|(_, v)| v.map_or("NULL".to_owned(), |v| v.to_string()))
        .collect();
    let sql = format!(
        "INSERT INTO {} (name, {}) VALUES ('r', {}) RETURNING id",
        spec.table,
        names.join(", "),
        values.join(", ")
    );
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

/// admit exactly what their owners admit, for two active sets; orphan owners are the installer's
/// probe, INSTALL's)
#[tokio::test]
async fn owner_chains_admit_what_the_owner_admits() {
    let (h, policy) = harness().await;
    let reg = Arc::clone(h.registry());
    let m = |short: &str| reg.model(&format!("{P}{short}")).unwrap();
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    // documents (form 1) and owners (form 2) of companies B, R, F
    let docs = seed_records(&mut tx, m("m1"), &[vec![B], vec![R], vec![F]]).await;
    let owners = seed_records(&mut tx, m("m2"), &[vec![B], vec![R], vec![F]]).await;
    let mut lines = Vec::new(); // (line id, doc companies)
    let mut sublines = Vec::new();
    for (doc, companies) in &docs {
        let line = insert_child(&mut tx, m("l"), &[("doc_id", Some(*doc))]).await;
        lines.push((line, companies.clone()));
        let sub = insert_child(&mut tx, m("l2"), &[("line_id", Some(line))]).await;
        sublines.push((sub, companies.clone()));
    }
    // repartitions: every own company (B, R, none) against every owner
    let mut parts = Vec::new(); // (id, own company, owner company)
    for own in [Some(B), Some(R), Some(F), None] {
        for (owner, oc) in &owners {
            let id = insert_child(
                &mut tx,
                m("p"),
                &[("company_id", own), ("owner_id", Some(*owner))],
            )
            .await;
            parts.push((id, own, oc[0]));
        }
    }
    tx.commit().await.unwrap();

    for active in [vec![B], vec![B, F]] {
        let mut tx = h
            .orm()
            .begin_opts(BeginOptions::for_principal(principal(&*policy, &active)))
            .await
            .unwrap();
        // l and l2 follow the document's company (form 1: exact, deny)
        for (model, rows) in [("l", &lines), ("l2", &sublines)] {
            let want: BTreeSet<i64> = rows
                .iter()
                .filter(|(_, c)| admits(false, false, false, c, &active))
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(
                visible(&mut tx, &format!("{P}{model}")).await,
                want,
                "{model} {active:?}"
            );
        }
        // p: its own company (ancestor, allow) and its owner's (ancestor, deny)
        let want: BTreeSet<i64> = parts
            .iter()
            .filter(|(_, own, oc)| {
                let own: Vec<i64> = own.iter().copied().collect();
                admits(false, true, true, &own, &active)
                    && admits(false, true, false, &[*oc], &active)
            })
            .map(|(id, _, _)| *id)
            .collect();
        assert_eq!(
            visible(&mut tx, &format!("{P}p")).await,
            want,
            "p {active:?}"
        );
        assert!(
            !want.is_empty() && (active.len() > 1 || want.len() < parts.len()),
            "p {active:?}: want {want:?} of {} rows",
            parts.len()
        );
    }
}

/// `parent_of company_ids`, the product template-line shape)
/// A plain search, `search_count`, a `read_group` count and searches through the edge itself
/// (`owner_id.name`, `owner_id any`), which inline the owner's own read rule, as a user in the
/// branch B and a user in its parent R: exactly the repartitions the restriction admits.
#[tokio::test]
async fn parent_edge_with_an_ancestor_owner_through_every_entry_point() {
    let (h, policy) = harness().await;
    let reg = Arc::clone(h.registry());
    let m = |short: &str| reg.model(&format!("{P}{short}")).unwrap();
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    let owners = seed_records(&mut tx, m("m2"), &[vec![B], vec![R], vec![S], vec![F]]).await;
    let mut parts = Vec::new(); // (id, owner company)
    for (owner, oc) in &owners {
        let id = insert_child(
            &mut tx,
            m("p"),
            &[("company_id", None), ("owner_id", Some(*owner))],
        )
        .await;
        parts.push((id, oc[0]));
    }
    tx.commit().await.unwrap();
    let model = format!("{P}p");
    let by_edge = [
        (
            "owner_id.name",
            Domain::condition("owner_id.name", erp_sdk::Operator::Eq, "r").unwrap(),
        ),
        (
            "owner_id any",
            Domain::condition(
                "owner_id",
                erp_sdk::Operator::Any,
                Domain::condition("name", erp_sdk::Operator::Eq, "r").unwrap(),
            )
            .unwrap(),
        ),
    ];
    for (active, who) in [(vec![B], "branch user in B"), (vec![R], "parent user in R")] {
        let want: BTreeSet<i64> = parts
            .iter()
            .filter(|(_, oc)| admits(false, true, false, &[*oc], &active))
            .map(|(id, _)| *id)
            .collect();
        assert!(
            !want.is_empty() && want.len() < parts.len(),
            "{who}: not vacuous: {want:?}"
        );
        let mut tx = h
            .orm()
            .begin_opts(BeginOptions::for_principal(principal(&*policy, &active)))
            .await
            .unwrap();
        let records = tx.env().records(&model).unwrap();
        // (i) a plain search
        assert_eq!(visible(&mut tx, &model).await, want, "{who}: search");
        // (j) search_count
        let count = records
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap_or_else(|e| panic!("{who}: search_count: {e}"));
        assert_eq!(
            usize::try_from(count).unwrap(),
            want.len(),
            "{who}: search_count"
        );
        // (k) a read_group count
        let groups = records
            .read_group(
                &mut tx,
                ReadGroup::new(Domain::TRUE).aggregates(["__count"]),
            )
            .await
            .unwrap_or_else(|e| panic!("{who}: read_group: {e}"));
        assert_eq!(
            format!("{groups:?}"),
            format!(
                "{:?}",
                vec![vec![erp_fields::Value::Int(
                    i64::try_from(want.len()).unwrap()
                )]]
            ),
            "{who}: read_group"
        );
        // through the edge: the owner's own rule applies inside the condition
        for (label, domain) in &by_edge {
            tx.capture_statements(true);
            let found = records.search(&mut tx, domain).await;
            let statements = tx.captured().len();
            tx.capture_statements(false);
            let found: BTreeSet<i64> = found
                .unwrap_or_else(|e| panic!("{who}: {label}: {e}"))
                .ids()
                .into_iter()
                .collect();
            eprintln!("{who}: {label}: {statements} statement(s)");
            assert_eq!(found, want, "{who}: {label}");
            assert!(statements <= 1, "{who}: {label}: {statements} statements");
        }
        drop(tx);
    }
}

#[test]
fn forms_a_to_e_are_references_texts() {
    let reg = registry();
    let all = generate(&reg).unwrap();
    // (fixture model, line of the csv, the row's domain column)
    let odoo = [
        ("m1", 10, "[('company_id', 'in', company_ids)]"),
        ("m2", 18, "[('company_id', 'parent_of', company_ids)]"),
        (
            "m3",
            4,
            "['|', ('company_id', '=', False), ('company_id', 'parent_of', company_ids)]",
        ),
        ("m4", 25, "[('company_id', 'in', company_ids + [False])]"),
        ("m5", 58, "[('company_ids', 'parent_of', company_ids)]"),
    ];
    for (short, line, text) in odoo {
        let model = format!("{P}{short}");
        let want = erp_security::company_access::compile_for_tests(&reg, &model, text).unwrap();
        let g = all.iter().find(|g| g.model_name() == model).unwrap();
        let got = erp_security::company_access::compile_for_tests(&reg, &model, g.text()).unwrap();
        assert_eq!(got, want, "{short} against csv line {line}");
        assert!(got.1, "{short} names the company");
    }
}
