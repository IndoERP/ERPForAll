//! `direct(…, escape = …)` over PostgreSQL: the generated
//! text, the admission of escaped and company-tested rows through the real policy compiler, the
//! stored-compute escape after a pending change in the same transaction, and a NULL escape column
//! being impossible. Expected admissions come from the statement in `admits`, not from the text.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::{Arc, PoisonError, RwLock};

use erp_orm::{
    AccessPolicy, BeginOptions, BoxFuture, CompanyId, GroupId, PolicyProvider, Principal,
    PrincipalBuilder, QualifiedXmlId, Resolution, Transaction, UpsertCall, UserId,
};
use erp_registry::{Registry, RegistryBuilder};
use erp_sdk::{Cell, Command, Domain, RawVals, RecId};
use erp_security::company_access::generate;
use erp_security::rows::{GroupRow, Implication, PolicyRows, RuleId, RuleRow};
use erp_security::snapshot::Compiler;
use erp_test_models::test_company_escape;
use erp_testkit::Harness;

const DOC: &str = "test_company_escape.doc";
const PLAIN: &str = "test_company_escape.plain";
const MEMBER: &str = "test_company_escape.member";
const B: i64 = 2;
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
        RegistryBuilder::new([&test_company_escape::ADDON])
            .build()
            .unwrap(),
    )
}

fn rows(registry: &Registry) -> PolicyRows {
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
    // companies are read by the many2many writes of `test_company_escape.member`
    builder = builder.rule(
        RuleRow::new(RuleId::new(id).unwrap(), "perm company", "res.company", "r")
            .unwrap()
            .with_group(gid(GROUP)),
    );
    builder.build().unwrap()
}

async fn harness() -> (Harness, Arc<dyn AccessPolicy>) {
    let open: Arc<dyn AccessPolicy> = Arc::new(
        Compiler::new(registry())
            .compile(&PolicyRows::builder(0).build().unwrap())
            .unwrap(),
    );
    let swap = Arc::new(Swap(RwLock::new(Arc::clone(&open))));
    let provider: Arc<dyn PolicyProvider> = swap.clone();
    let h = Harness::builder([&test_company_escape::ADDON])
        .policy(provider)
        .build()
        .await
        .expect("harness");
    let policy: Arc<dyn AccessPolicy> = Arc::new(
        Compiler::new(Arc::clone(h.registry()))
            .compile(&rows(h.registry()))
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
            Resolution::in_id_order(groups, [1, B, 3, F].iter().map(company), NonZeroU64::MIN)
                .unwrap(),
        )
        .companies(active.iter().map(company).collect())
        .build()
        .unwrap()
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

/// The admission rule stated on values: a record with `escape == false` is admitted; any other
/// takes the company test (`allow` admits a record without company).
fn admits(escape: bool, company: Option<i64>, allow: bool, active: &[i64]) -> bool {
    !escape
        || match company {
            None => allow,
            Some(c) => active.contains(&c),
        }
}

#[test]
fn generated_text_and_row_name() {
    let all = generate(&registry()).unwrap();
    let get = |m: &str| all.iter().find(|g| g.model_name() == m).unwrap();
    assert_eq!(
        get(DOC).text(),
        "['|', ('share', '=', False), ('company_id', 'in', company_ids)]"
    );
    assert_eq!(
        get(DOC).name(),
        "Company restriction of test_company_escape.doc (company_id, exact, deny, escape share)"
    );
    assert_eq!(
        get(PLAIN).text(),
        "['|', ('flag', '=', False), ('company_id', 'in', company_ids + [False])]"
    );
}

/// plain stored escape, for the active company B; groups and the owner chains are other tests)
#[tokio::test]
async fn escape_admits_false_and_applies_the_company_test_otherwise() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    // (kind, company): `share` is computed from `kind` and stored
    let docs = [
        ("user", Some(F)),
        ("portal", Some(F)),
        ("portal", Some(B)),
        ("user", Some(B)),
        ("portal", None),
        ("user", None),
    ];
    let mut doc_ids = Vec::new();
    for (kind, company) in docs {
        let company = company.map_or("NULL".to_owned(), |c| c.to_string());
        // inserted with `share` as the stored compute would write it
        let sql = format!(
            "INSERT INTO test_company_escape_doc (name, kind, share, company_id) \
             VALUES ('d{}', '{kind}', {}, {company}) RETURNING id",
            doc_ids.len(),
            kind == "portal"
        );
        let id: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);
        doc_ids.push((id, kind == "portal", docs_company(&company)));
    }
    let plains = [
        (false, Some(F)),
        (true, Some(F)),
        (true, Some(B)),
        (true, None),
    ];
    let mut plain_ids = Vec::new();
    for (flag, company) in plains {
        let c = company.map_or("NULL".to_owned(), |c| c.to_string());
        let sql = format!(
            "INSERT INTO test_company_escape_plain (name, flag, company_id) \
             VALUES ('p', {flag}, {c}) RETURNING id"
        );
        let id: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);
        plain_ids.push((id, flag, company));
    }
    tx.commit().await.unwrap();

    let active = [B];
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &active)))
        .await
        .unwrap();
    let seen = |ids: Vec<i64>| ids.into_iter().collect::<BTreeSet<_>>();
    let found = seen(
        tx.env()
            .records(DOC)
            .unwrap()
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .ids(),
    );
    let want: BTreeSet<i64> = doc_ids
        .iter()
        .filter(|(_, share, company)| admits(*share, *company, false, &active))
        .map(|(id, ..)| *id)
        .collect();
    assert_eq!(found, want, "{DOC}");
    let found = seen(
        tx.env()
            .records(PLAIN)
            .unwrap()
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .ids(),
    );
    let want: BTreeSet<i64> = plain_ids
        .iter()
        .filter(|(_, flag, company)| admits(*flag, *company, true, &active))
        .map(|(id, ..)| *id)
        .collect();
    assert_eq!(found, want, "{PLAIN}");
    // not vacuous: some rows are admitted by the escape alone, some hidden
    assert!(!want.is_empty() && want.len() < plain_ids.len());
}

fn docs_company(text: &str) -> Option<i64> {
    text.parse().ok()
}

/// one; the registry refusal of an optional escape is in the registry tests)
#[tokio::test]
async fn escape_column_refuses_null() {
    let (h, _policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    for (table, columns, field) in [
        (
            "test_company_escape_doc",
            "(name, share) VALUES ('x', NULL)",
            "Share",
        ),
        (
            "test_company_escape_plain",
            "(name, flag) VALUES ('x', NULL)",
            "Flag",
        ),
    ] {
        let err = tx
            .execute(&format!("INSERT INTO {table} {columns}"), &[])
            .await
            .unwrap_err();
        assert!(
            err.message()
                .contains(&format!("Missing required field '{field}'")),
            "{table}: {}",
            err.message()
        );
        // the failed statement aborts the transaction; start a fresh one
        tx = h.begin().await.unwrap();
    }
}

/// restricted search of the same transaction, in both directions; the group-driven compute of
/// `res.users.share` comes with the users step)
#[tokio::test]
async fn pending_recompute_of_the_escape_is_seen_by_the_next_search() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    // a portal-kind (share = true) document of the foreign company F: hidden for active [B]
    let id: i64 = tx
        .query(
            "INSERT INTO test_company_escape_doc (name, kind, share, company_id) \
             VALUES ('d', 'portal', true, 4) RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &[B])))
        .await
        .unwrap();
    assert!(!search_ids(&mut tx).await.contains(&id), "hidden before");
    // flip `kind` as the superuser: `share` is recomputed (pending), nothing flushed by hand
    tx.env()
        .records(DOC)
        .unwrap()
        .browse([id])
        .unwrap()
        .sudo(true)
        .write(
            &mut tx,
            RawVals::new().set_cell("kind", Cell::Str("user".into())),
        )
        .await
        .unwrap();
    assert!(
        search_ids(&mut tx).await.contains(&id),
        "visible once share is false"
    );
    tx.env()
        .records(DOC)
        .unwrap()
        .browse([id])
        .unwrap()
        .sudo(true)
        .write(
            &mut tx,
            RawVals::new().set_cell("kind", Cell::Str("portal".into())),
        )
        .await
        .unwrap();
    assert!(
        !search_ids(&mut tx).await.contains(&id),
        "hidden again once share is true"
    );
    tx.rollback().await.unwrap();
}

async fn search_ids(tx: &mut Transaction) -> BTreeSet<i64> {
    tx.env()
        .records(DOC)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids()
        .into_iter()
        .collect()
}

/// an escape model, escaped rows included, and a read by id is an access error)
#[tokio::test]
async fn an_empty_active_company_set_admits_nothing_even_through_the_escape() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    let doc: i64 = tx
        .query(
            "INSERT INTO test_company_escape_doc (name, kind, share, company_id) \
             VALUES ('d', 'user', false, 4) RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.execute(
        "INSERT INTO test_company_escape_plain (name, flag, company_id) VALUES ('p', false, NULL)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // control: with B active the escaped document is visible
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &[B])))
        .await
        .unwrap();
    assert!(search_ids(&mut tx).await.contains(&doc));
    tx.rollback().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &[])))
        .await
        .unwrap();
    assert!(search_ids(&mut tx).await.is_empty(), "{DOC}");
    let plain = tx
        .env()
        .records(PLAIN)
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    assert!(plain.ids().is_empty(), "{PLAIN}");
    let err = tx
        .env()
        .records(DOC)
        .unwrap()
        .browse([doc])
        .unwrap()
        .read(&mut tx, &["name"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Access, "{err}");
}

/// new-state restriction and exact Access error as ordinary write, with poisoning and rollback)
#[tokio::test]
async fn upsert_of_escape_dependency_checks_the_new_company_restriction() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    let id: i64 = tx
        .query(
            "INSERT INTO test_company_escape_doc (name, kind, share, company_id) \
             VALUES ('foreign', 'user', false, 4) RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();

    // The established ordinary-write path supplies the complete policy error for this exact
    // record/principal. Comparing the two real entry points avoids a hand-built explanation.
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &[B])))
        .await
        .unwrap();
    let expected = tx
        .env()
        .records(DOC)
        .unwrap()
        .browse([id])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("kind", Cell::Str("portal".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(expected.kind(), erp_fields::ErrorKind::Access);
    tx.rollback().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &[B])))
        .await
        .unwrap();
    let call = UpsertCall::new("_name_key")
        .with_rows([RawVals::new()
            .set_cell("name", Cell::Str("foreign".into()))
            .set_cell("kind", Cell::Str("portal".into()))])
        .with_update(["kind"]);
    let err = tx
        .env()
        .records(DOC)
        .unwrap()
        .upsert(&mut tx, call)
        .await
        .expect_err("a computed escape must not let upsert move a record outside its restriction");
    assert_eq!(err.kind(), expected.kind());
    assert_eq!(err.message(), expected.message());
    assert!(tx.is_poisoned());
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.kind(), err.kind());
    assert_eq!(commit.message(), err.message());
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let row = tx
        .query(
            "SELECT kind, share FROM test_company_escape_doc WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(row[0].get::<_, String>(0), "user");
    assert!(!row[0].get::<_, bool>(1));
}

/// company state is permitted; an ordinary direct escape write refuses a foreign new state)
#[tokio::test]
async fn escape_write_checks_keep_allowed_updates_and_direct_escape_controls() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    tx.execute(
        "INSERT INTO test_company_escape_doc (name, kind, share, company_id) \
         VALUES ('own', 'user', false, 2)",
        &[],
    )
    .await
    .unwrap();
    let plain: i64 = tx
        .query(
            "INSERT INTO test_company_escape_plain (name, flag, company_id) \
         VALUES ('foreign', false, 4) RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &[B])))
        .await
        .unwrap();
    tx.env()
        .records(DOC)
        .unwrap()
        .upsert(
            &mut tx,
            UpsertCall::new("_name_key")
                .with_rows([RawVals::new()
                    .set_cell("name", Cell::Str("own".into()))
                    .set_cell("kind", Cell::Str("portal".into()))])
                .with_update(["kind"]),
        )
        .await
        .unwrap();
    let read = tx
        .env()
        .records(DOC)
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap()
        .read(&mut tx, &["share"])
        .await
        .unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].get("share"), Some(&erp_fields::Value::Bool(true)));
    assert!(!tx.is_poisoned());
    tx.commit().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &[B])))
        .await
        .unwrap();
    let records = tx.env().records(PLAIN).unwrap().browse([plain]).unwrap();
    // Read succeeds solely through the escape, then setting it true must refuse the new state.
    records.read(&mut tx, &["flag"]).await.unwrap();
    let err = records
        .write(&mut tx, RawVals::new().set_cell("flag", Cell::Bool(true)))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        err.message(),
        "Access rules protect some of these records.\n\ndemo (id=2) has no 'write' access to:\n- Escape over a plain boolean (test_company_escape.plain)\n\nAsk an administrator if you need access to these records.\n\nTo open the record, switch to its company: F."
    );
    assert!(tx.is_poisoned());
    tx.rollback().await.unwrap();
}

fn member(name: &str, note: &str, company: i64) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("note", Cell::Str(note.into()))
        .set_commands(
            "company_ids",
            [Command::set([RecId::from_real(company).unwrap()])],
        )
}

/// A record created in a company that is not active is
/// still refused, by that read.
#[tokio::test]
async fn an_inverse_reads_the_new_record_after_its_many2many_values() {
    let (h, policy) = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed_companies(&mut tx).await;
    tx.commit().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &[B])))
        .await
        .unwrap();
    let created = tx
        .env()
        .records(MEMBER)
        .unwrap()
        .create(&mut tx, vec![member("in B", "kept", B)])
        .await
        .unwrap();
    let read = created
        .read(&mut tx, &["stored_note", "company_ids"])
        .await
        .unwrap();
    assert_eq!(
        read[0].get("stored_note"),
        Some(&erp_fields::Value::Str("kept".into()))
    );
    assert_eq!(
        read[0].get("company_ids"),
        Some(&erp_fields::Value::list([erp_fields::Value::Int(B)]))
    );
    tx.commit().await.unwrap();

    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(&*policy, &[B])))
        .await
        .unwrap();
    let err = tx
        .env()
        .records(MEMBER)
        .unwrap()
        .create(&mut tx, vec![member("in F", "lost", F)])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        err.message(),
        "Access rules protect some of these records.\n\ndemo (id=2) has no 'read' access to:\n- Member restricted through its companies (test_company_escape.member)\n\nAsk an administrator if you need access to these records.\n\nTo open the record, switch to its company: F."
    );
    tx.rollback().await.unwrap();
}
