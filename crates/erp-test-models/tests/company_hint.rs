#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::num::NonZeroU64;
use std::sync::Arc;

use erp_fields::ErrorKind;
use erp_orm::{
    BeginOptions, CompanyId, GroupId, Operation, Orm, PolicyProvider, PrincipalBuilder, Records,
    Resolution, Transaction, UserId,
};
use erp_registry::{AddonDescriptor, CompanyAccessDecl, FieldDecl, ModelDecl};
use erp_sdk::Value;
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};

static FIXTURE: AddonDescriptor = AddonDescriptor {
    name: "x_company",
    depends: &["base"],
    exact_only: false,
};

const DOC: &str = "x.company_doc";
const SHARED: &str = "x.company_shared";
const CHILD: &str = "x.company_child";

const DOC_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::many2one("company_id", "res.company"),
];
const SHARED_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::many2many("company_ids", "res.company"),
];
const CHILD_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::many2one("parent_id", DOC),
];

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &FIXTURE,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("x_company", DOC)
            .company_access(CompanyAccessDecl::unrestricted(
                "company hint fixture tests handwritten rules",
            ))
            .description("Company Document")
            .fields(DOC_FIELDS),
        ModelDecl::new("x_company", SHARED)
            .company_access(CompanyAccessDecl::unrestricted(
                "company hint fixture tests handwritten rules",
            ))
            .description("Company Shared")
            .fields(SHARED_FIELDS),
        ModelDecl::new("x_company", CHILD)
            .description("Company Child")
            .fields(CHILD_FIELDS),
    ])
    .build()
    .await
    .expect("harness")
}

fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("company_hint: connection closed with an error: {error}");
        }
    });
    client
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

/// Everything the cases need, committed.
struct World {
    bob: i64,
    /// `base.group_user`, Group 1, `base.group_no_one`.
    groups: [i64; 3],
    group1: i64,
    doc_model: i64,
    child_model: i64,
    /// A root company, its branch (where Bob works), a company Bob is in, and one he is not.
    root: i64,
    branch: i64,
    member: i64,
    foreign: i64,
}

async fn company(client: &Client, name: &str, parent: Option<i64>, currency: i64) -> i64 {
    one(
        client,
        "INSERT INTO res_company (name, parent_id, currency_id) VALUES ($1, $2, $3) RETURNING id::int8",
        &[&name, &parent, &currency],
    )
    .await
}

async fn seed(client: &Client) -> World {
    let currency = erp_testkit::fixtures::seed_currency(client).await.unwrap();
    let group = async |name: &str| -> i64 {
        one(
            client,
            "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
            &[&name],
        )
        .await
    };
    let internal = group("Internal User").await;
    let group1 = group("Group 1").await;
    let no_one = group("Technical Features").await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', 'group_user', 'res.groups', $1), \
                    ('base', 'group_no_one', 'res.groups', $2)",
            &[&internal, &no_one],
        )
        .await
        .unwrap();
    let bob = one(
        client,
        "INSERT INTO res_users (name, login, active, share) \
         VALUES ('Bob Bobman', 'bob', true, false) RETURNING id::int8",
        &[],
    )
    .await;
    for gid in [internal, group1, no_one] {
        client
            .execute(
                "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
                &[&bob, &gid],
            )
            .await
            .unwrap();
    }
    let model = async |name: &str, model: &str| -> i64 {
        one(
            client,
            "INSERT INTO ir_model (name, model, state, transient) \
             VALUES ($1, $2, 'base', false) RETURNING id::int8",
            &[&name, &model],
        )
        .await
    };
    let doc_model = model("Company Document", DOC).await;
    let child_model = model("Company Child", CHILD).await;
    let root = company(client, "Company Root", None, currency).await;
    let branch = company(client, "Company Branch", Some(root), currency).await;
    let member = company(client, "Company B", None, currency).await;
    let foreign = company(client, "Company C", None, currency).await;
    World {
        bob,
        groups: [internal, group1, no_one],
        group1,
        doc_model,
        child_model,
        root,
        branch,
        member,
        foreign,
    }
}

async fn doc(client: &Client, name: &str, company: Option<i64>) -> i64 {
    one(
        client,
        "INSERT INTO x_company_doc (name, company_id) VALUES ($1, $2) RETURNING id::int8",
        &[&name, &company],
    )
    .await
}

/// Replace the access rows of `model`: `(name, domain, with Group 1, operations)`.
async fn set_rules(
    client: &Client,
    w: &World,
    model: i64,
    rules: &[(&str, Option<&str>, bool, &str)],
) {
    client
        .execute("DELETE FROM ir_access WHERE model_id = $1", &[&model])
        .await
        .unwrap();
    for (name, domain, with_group, operations) in rules {
        let group = with_group.then_some(w.group1);
        client
            .execute(
                "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
                 VALUES ($1, $2, $3, $4, $5, true)",
                &[name, &model, &group, operations, domain],
            )
            .await
            .unwrap();
    }
}

async fn policy_orm(h: &Harness) -> (Orm, Arc<PolicySource>) {
    let source = Arc::new(
        PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
            .await
            .unwrap(),
    );
    let provider: Arc<dyn PolicyProvider> = Arc::clone(&source) as Arc<dyn PolicyProvider>;
    let orm = Orm::builder(Arc::clone(h.registry()), pool(h, 2))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();
    (orm, source)
}

fn company_id(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

/// Bob's membership as the database holds it (the engine checks a bound subject against it).
async fn set_membership(client: &Client, w: &World, member_of: &[i64]) {
    client
        .execute(
            "DELETE FROM res_company_users_rel WHERE user_id = $1",
            &[&w.bob],
        )
        .await
        .unwrap();
    for company in member_of {
        client
            .execute(
                "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
                &[&w.bob, company],
            )
            .await
            .unwrap();
    }
    client
        .execute(
            "UPDATE res_users SET company_id = $2 WHERE id = $1",
            &[&w.bob, &member_of[0]],
        )
        .await
        .unwrap();
}

/// Bob, a member of `member_of`, working in `active`, in debug mode when `debug`.
async fn begin_bob(
    client: &Client,
    orm: &Orm,
    source: &PolicySource,
    w: &World,
    member_of: &[i64],
    active: &[i64],
    debug: bool,
) -> Transaction {
    set_membership(client, w, member_of).await;
    let direct: Vec<GroupId> = w.groups.iter().map(|g| GroupId::new(*g).unwrap()).collect();
    let groups = source.current().groups(&direct).unwrap();
    let resolution = Resolution::in_id_order(
        groups,
        member_of.iter().map(|c| company_id(*c)),
        NonZeroU64::MIN,
    )
    .unwrap()
    .with_default_order(member_of.iter().map(|c| company_id(*c)).collect())
    .unwrap();
    let principal = PrincipalBuilder::user(UserId::new(w.bob).unwrap())
        .resolution(resolution)
        .companies(active.iter().map(|c| company_id(*c)).collect())
        .debug(debug)
        .build()
        .unwrap();
    orm.begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap()
}

fn head(w: &World, op: &str) -> String {
    format!(
        "Access rules protect some of these records.\n\nBob Bobman (id={}) has no '{op}' access to:",
        w.bob
    )
}

const COOKIES: &str = "Ask an administrator if you need access to these records.";
const OUTSIDE: &str = "The record belongs to a company outside your allowed companies.";
const AMBIGUOUS: &str =
    "The records belong to several companies: switching the current company may help.";

fn switch_to(name: &str) -> String {
    format!("To open the record, switch to its company: {name}.")
}

fn records(tx: &Transaction, model: &str, ids: &[i64]) -> Records {
    tx.env()
        .records(model)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}

/// The context's suggested company as `(id, display_name)`.
fn suggested(error: &erp_fields::Error) -> Option<(i64, String)> {
    let Value::Map(outer) = error.context()? else {
        panic!("not a map: {:?}", error.context());
    };
    let Some(Value::Map(inner)) = outer.get("suggested_company") else {
        panic!("no suggested_company in {outer:?}");
    };
    let (Some(Value::Int(id)), Some(Value::Str(name))) =
        (inner.get("id"), inner.get("display_name"))
    else {
        panic!("malformed suggested_company {inner:?}");
    };
    Some((*id, name.to_string()))
}

async fn denied(
    tx: &mut Transaction,
    model: &str,
    ids: &[i64],
    op: Operation,
) -> erp_fields::Error {
    let records = records(tx, model, ids);
    records.check_access(tx, op).await.unwrap_err()
}

/// The statements one denied read of `id` costs once the policy is warm: the same call is made
/// twice and the second is counted.
async fn denied_cost(tx: &mut Transaction, model: &str, id: i64) -> u64 {
    denied(tx, model, &[id], Operation::Read).await;
    let before = tx.query_count();
    denied(tx, model, &[id], Operation::Read).await;
    tx.query_count() - before
}

/// The hint rides on the evidence statement:
/// the error path costs the same statements with or without a company on the model.
#[tokio::test]
async fn spec_sec_010_multi_company_hint() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let cookies_hint = |hint: &str| format!("{COOKIES}\n\n{hint}");
    let both = [w.root, w.member];

    // test_warn_company_access: Bob is in the record's company but works in another
    let in_b = doc(&client, "In B", Some(w.member)).await;
    set_rules(
        &client,
        &w,
        w.doc_model,
        &[(
            "rule 0",
            Some("[('company_id', '=', user.company_id.id)]"),
            true,
            "r",
        )],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    for debug in [true, false] {
        let mut tx = begin_bob(&client, &orm, &source, &w, &both, &both, debug).await;
        let err = denied(&mut tx, DOC, &[in_b], Operation::Read).await;
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");
        let expected = if debug {
            format!(
                "{}\n- Company Document, In B ({DOC}: {in_b}, company=Company B)\n\nRules that refused it:\n- rule 0\n\n{}",
                head(&w, "read"),
                cookies_hint(&switch_to("Company B"))
            )
        } else {
            format!(
                "{}\n- Company Document ({DOC})\n\n{}",
                head(&w, "read"),
                cookies_hint(&switch_to("Company B"))
            )
        };
        assert_eq!(err.message(), expected, "debug: {debug}");
        assert_eq!(suggested(&err), Some((w.member, "Company B".to_owned())));
        drop(tx);
    }

    // the spec's Given: a record of company B readable only through `company_id in company_ids`;
    // the user is allowed B but works in A (review RC7).
    set_rules(
        &client,
        &w,
        w.doc_model,
        &[(
            "rule 0",
            Some("[('company_id', 'in', company_ids)]"),
            true,
            "r",
        )],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    for debug in [true, false] {
        let mut tx = begin_bob(&client, &orm, &source, &w, &both, &[w.root], debug).await;
        let err = denied(&mut tx, DOC, &[in_b], Operation::Read).await;
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");
        let expected = if debug {
            format!(
                "{}\n- Company Document ({DOC}: {in_b}, company=Company B)\n\nRules that refused it:\n- rule 0\n\n{}",
                head(&w, "read"),
                cookies_hint(&switch_to("Company B"))
            )
        } else {
            format!(
                "{}\n- Company Document ({DOC})\n\n{}",
                head(&w, "read"),
                cookies_hint(&switch_to("Company B"))
            )
        };
        assert_eq!(err.message(), expected, "debug: {debug}");
        assert_eq!(suggested(&err), Some((w.member, "Company B".to_owned())));
        drop(tx);
    }

    // test_warn_company_no_access: the record has no company, so there is nothing to suggest
    let none = doc(&client, "No company", None).await;
    set_rules(
        &client,
        &w,
        w.doc_model,
        &[(
            "rule 0",
            Some("[('company_id', '=', user.company_id.id)]"),
            true,
            "u",
        )],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    for debug in [true, false] {
        let mut tx = begin_bob(&client, &orm, &source, &w, &both, &both, debug).await;
        let err = denied(&mut tx, DOC, &[none], Operation::Write).await;
        let expected = if debug {
            format!(
                "{}\n- Company Document, No company ({DOC}: {none})\n\nRules that refused it:\n- rule 0\n\n{COOKIES}",
                head(&w, "write")
            )
        } else {
            format!(
                "{}\n- Company Document ({DOC})\n\n{COOKIES}",
                head(&w, "write")
            )
        };
        assert_eq!(err.message(), expected, "debug: {debug}");
        assert!(err.context().is_none());
        drop(tx);
    }

    // test_warn_company_access_multi_record: two companies among the denied records
    let in_b2 = doc(&client, "In B again", Some(w.member)).await;
    let in_root = doc(&client, "In Root", Some(w.root)).await;
    set_rules(
        &client,
        &w,
        w.doc_model,
        &[("rule 0", Some("[('company_id', '=', False)]"), true, "r")],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    for debug in [true, false] {
        let mut tx = begin_bob(&client, &orm, &source, &w, &both, &both, debug).await;
        let err = denied(&mut tx, DOC, &[in_b2, in_root], Operation::Read).await;
        let expected = if debug {
            format!(
                "{}\n- Company Document, In B again ({DOC}: {in_b2}, company=Company B)\n- \
                 Company Document, In Root ({DOC}: {in_root}, company=Company Root)\n\nRules that refused it:\n- rule 0\n\n{}",
                head(&w, "read"),
                cookies_hint(AMBIGUOUS)
            )
        } else {
            format!(
                "{}\n- Company Document ({DOC})\n\n{}",
                head(&w, "read"),
                cookies_hint(AMBIGUOUS)
            )
        };
        assert_eq!(err.message(), expected, "debug: {debug}");
        assert!(err.context().is_none(), "several companies suggest none");
        drop(tx);
    }

    // test_warn_company_no_company_field: the rule mentions company_id through a path, the
    // model has none
    let child = one(
        &client,
        "INSERT INTO x_company_child (name, parent_id) VALUES ('Kid', $1) RETURNING id::int8",
        &[&in_b],
    )
    .await;
    set_rules(
        &client,
        &w,
        w.child_model,
        &[(
            "rule 0",
            Some("[('parent_id.company_id', '=', user.company_id.id)]"),
            true,
            "r",
        )],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    let mut tx = begin_bob(&client, &orm, &source, &w, &both, &both, true).await;
    let err = denied(&mut tx, CHILD, &[child], Operation::Read).await;
    assert_eq!(
        err.message(),
        format!(
            "{}\n- Company Child, Kid ({CHILD}: {child})\n\nRules that refused it:\n- rule \
             0\n\n{COOKIES}",
            head(&w, "read")
        )
    );
    drop(tx);

    // the hint costs nothing: the whole error path of a company model costs the statements of the
    // company-less model's (review RC8), whatever the mode
    for debug in [false, true] {
        let mut tx = begin_bob(&client, &orm, &source, &w, &both, &both, debug).await;
        let with_company = denied_cost(&mut tx, DOC, in_b).await;
        let without = denied_cost(&mut tx, CHILD, child).await;
        assert!(without >= 1, "the denied read runs statements");
        assert_eq!(with_company, without, "debug: {debug}");
        drop(tx);
    }
    // and the evidence statement of a company model is the same one statement as the company-less
    // model's, and it carries the companies
    let mut tx = begin_bob(&client, &orm, &source, &w, &both, &both, false).await;
    tx.capture_statements(true);
    denied(&mut tx, DOC, &[in_b], Operation::Read).await;
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    let evidence: Vec<&String> = statements
        .iter()
        .filter(|sql| sql.contains("WITH ORDINALITY"))
        .collect();
    assert_eq!(evidence.len(), 1, "{statements:#?}");
    assert!(evidence[0].contains("res_company"), "{}", evidence[0]);
    assert_eq!(
        statements
            .iter()
            .filter(|sql| sql.contains("\"res_company\""))
            .count(),
        1,
        "the companies are read by the evidence statement only: {statements:#?}"
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// A global restriction `['|', ('company_id', 'parent_of', company_ids), ('company_id', '=',
/// False)]`: Bob works in a branch company and sees its records, its parent's and those of no
/// company, not those of other companies (search and read); by id, `write` and `unlink` of
/// another company's record are refused with the multi-company hint naming the company when Bob
/// is in it, and with the "do not have access" text when he is not.
#[tokio::test]
async fn spec_sec_054_multi_company_rules() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let in_branch = doc(&client, "Branch record", Some(w.branch)).await;
    let in_root = doc(&client, "Root record", Some(w.root)).await;
    let in_none = doc(&client, "Shared record", None).await;
    let in_b = doc(&client, "B record", Some(w.member)).await;
    let in_c = doc(&client, "C record", Some(w.foreign)).await;
    set_rules(
        &client,
        &w,
        w.doc_model,
        &[
            ("Group access", None, true, "crud"),
            (
                "Company rule",
                Some("['|', ('company_id', 'parent_of', company_ids), ('company_id', '=', False)]"),
                false,
                "crud",
            ),
        ],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    let member_of = [w.branch, w.member];
    for debug in [false, true] {
        let mut tx = begin_bob(&client, &orm, &source, &w, &member_of, &[w.branch], debug).await;
        let all = records(&tx, DOC, &[in_branch, in_root, in_none, in_b, in_c]);

        // search sees the branch, its parent and the company-less record
        let found = tx
            .env()
            .records(DOC)
            .unwrap()
            .search(&mut tx, &erp_sdk::Domain::TRUE)
            .await
            .unwrap();
        assert_eq!(found.ids(), [in_branch, in_root, in_none]);
        // the readable subset of a mixed set
        let readable = all.filtered_access(&mut tx, Operation::Read).await.unwrap();
        assert_eq!(readable.ids(), [in_branch, in_root, in_none]);
        // `search_read` returns the same three rows; `read` of a record of another company is
        // refused with the hint (review RC7: the read path, not only the filtered subset)
        let rows = tx
            .env()
            .records(DOC)
            .unwrap()
            .search_read(
                &mut tx,
                &erp_sdk::Domain::TRUE,
                &["name"],
                &erp_sdk::SearchOptions::new(),
            )
            .await
            .unwrap();
        let ids: Vec<Value> = rows.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(
            ids,
            [in_branch, in_root, in_none].map(Value::Int),
            "debug: {debug}"
        );
        let err = records(&tx, DOC, &[in_b])
            .read(&mut tx, &["name"])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");
        assert!(err.message().ends_with(&switch_to("Company B")), "{err}");
        assert_eq!(suggested(&err), Some((w.member, "Company B".to_owned())));
        let err = all.read(&mut tx, &["name"]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");

        // by id: B is Bob's company, so the error names it; C is not, so it is never named
        // B is not among the active companies, so the record is listed without its name
        let line =
            |id: i64, company: &str| format!("- Company Document ({DOC}: {id}, company={company})");
        for op in [Operation::Read, Operation::Write, Operation::Unlink] {
            let verb = match op {
                Operation::Read => "read",
                Operation::Write => "write",
                _ => "delete",
            };
            let err = denied(&mut tx, DOC, &[in_b], op).await;
            assert_eq!(err.kind(), ErrorKind::Access, "{err}");
            let expected = if debug {
                format!(
                    "{}\n{}\n\nRules that refused it:\n- Company rule\n\n{COOKIES}\n\n{}",
                    head(&w, verb),
                    line(in_b, "Company B"),
                    switch_to("Company B")
                )
            } else {
                format!(
                    "{}\n- Company Document ({DOC})\n\n{COOKIES}\n\n{}",
                    head(&w, verb),
                    switch_to("Company B")
                )
            };
            assert_eq!(err.message(), expected, "{verb} debug: {debug}");
            assert_eq!(suggested(&err), Some((w.member, "Company B".to_owned())));

            let err = denied(&mut tx, DOC, &[in_c], op).await;
            let expected = if debug {
                // C is not Bob's company: the line carries neither its name nor its company
                format!(
                    "{}\n- Company Document ({DOC}: {in_c})\n\nRules that refused it:\n- \
                     Company rule\n\n{COOKIES}\n\n{OUTSIDE}",
                    head(&w, verb)
                )
            } else {
                format!(
                    "{}\n- Company Document ({DOC})\n\n{COOKIES}\n\n{OUTSIDE}",
                    head(&w, verb)
                )
            };
            assert_eq!(err.message(), expected, "{verb} debug: {debug}");
            assert!(err.context().is_none());
        }
        // a real write and a real unlink take the same road
        let target = records(&tx, DOC, &[in_b]);
        let err = target
            .write_values(&mut tx, &support::map(&[("name", Value::str("x"))]))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");
        assert!(err.message().ends_with(&switch_to("Company B")), "{err}");
        let err = target.unlink(&mut tx).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");
        assert!(err.message().ends_with(&switch_to("Company B")), "{err}");
        drop(tx);
    }

    // working in the root company, the branch's records are out of reach (a descendant is no
    // ancestor): Bob is in the branch, so it is the one to switch to
    let mut tx = begin_bob(
        &client,
        &orm,
        &source,
        &w,
        &[w.root, w.branch],
        &[w.root],
        false,
    )
    .await;
    let found = tx
        .env()
        .records(DOC)
        .unwrap()
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(found.ids(), [in_root, in_none]);
    let err = denied(&mut tx, DOC, &[in_branch], Operation::Read).await;
    assert!(
        err.message().ends_with(&switch_to("Company Branch")),
        "{err}"
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// The debug listing shows a record's display name only when it has no company or a company among
/// the principal's allowed (active) companies; the others are listed by model and id, with
/// `company=` for the subject's own companies. The names of hidden records are never read: when
/// every record is hidden the error path runs no display-name statement.
#[tokio::test]
async fn debug_listing_redacts_foreign_names() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let active = doc(&client, "Active record", Some(w.root)).await;
    let other_member = doc(&client, "Member record", Some(w.member)).await;
    let foreign = doc(&client, "Foreign record", Some(w.foreign)).await;
    let nobody = doc(&client, "Nobody record", None).await;
    set_rules(
        &client,
        &w,
        w.doc_model,
        &[(
            "Company rule",
            Some("[('company_id', 'in', company_ids), ('name', '=', 'zz')]"),
            true,
            "r",
        )],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    let member_of = [w.root, w.member];
    let mut tx = begin_bob(&client, &orm, &source, &w, &member_of, &[w.root], true).await;
    let err = denied(
        &mut tx,
        DOC,
        &[active, other_member, foreign, nobody],
        Operation::Read,
    )
    .await;
    assert_eq!(
        err.message(),
        format!(
            "{}\n- Company Document, Active record ({DOC}: {active}, company=Company Root)\n- \
             Company Document ({DOC}: {other_member}, company=Company B)\n- Company Document \
             ({DOC}: {foreign})\n- Company Document, Nobody record ({DOC}: {nobody})\n\nRules that refused it:\n- Company rule\n\n{COOKIES}\n\n{AMBIGUOUS}",
            head(&w, "read")
        )
    );
    assert!(!err.message().contains("Foreign record"), "{err}");
    assert!(!err.message().contains("Company C"), "{err}");
    assert!(!err.message().contains("Member record"), "{err}");
    drop(tx);

    // every record hidden: the error path costs what the plain text costs, no display-name read
    let mut counts = Vec::new();
    for debug in [false, true] {
        let mut tx = begin_bob(&client, &orm, &source, &w, &member_of, &[w.root], debug).await;
        let before = tx.query_count();
        let err = denied(&mut tx, DOC, &[other_member, foreign], Operation::Read).await;
        counts.push(tx.query_count() - before);
        assert!(!err.message().contains("Member record"), "{err}");
        assert!(!err.message().contains("Foreign record"), "{err}");
        drop(tx);
    }
    assert_eq!(
        counts[1], counts[0],
        "debug {} vs plain {}",
        counts[1], counts[0]
    );
    // and with one readable record the names are read again
    let mut tx = begin_bob(&client, &orm, &source, &w, &member_of, &[w.root], true).await;
    let before = tx.query_count();
    denied(&mut tx, DOC, &[active, foreign], Operation::Read).await;
    assert_eq!(tx.query_count() - before, counts[0] + 1);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// A company the subject is not in is never named, in the hint, the listing or the context, in
/// debug and normal mode, for a single company and a company-less `company_ids` record; a shared
/// record suggests the first of the subject's companies among its own.
#[tokio::test]
async fn hint_never_names_foreign_company() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let foreign = doc(&client, "Foreign record", Some(w.foreign)).await;
    set_rules(
        &client,
        &w,
        w.doc_model,
        &[(
            "rule 0",
            Some("[('company_id', '=', user.company_id.id)]"),
            true,
            "r",
        )],
    )
    .await;
    let shared = one(
        &client,
        "INSERT INTO x_company_shared (name) VALUES ('Shared') RETURNING id::int8",
        &[],
    )
    .await;
    let spec = h.registry().model(SHARED).unwrap();
    let rel = match &spec.field("company_ids").unwrap().ty {
        erp_fields::FieldType::Many2many {
            relation: Some(rel),
            ..
        } => rel.clone(),
        other => panic!("not a stored many2many: {other:?}"),
    };
    for company in [w.foreign, w.member] {
        client
            .execute(
                &format!(
                    "INSERT INTO \"{}\" (\"{}\", \"{}\") VALUES ($1, $2)",
                    rel.table, rel.column1, rel.column2
                ),
                &[&shared, &company],
            )
            .await
            .unwrap();
    }
    let shared_model = one(
        &client,
        "INSERT INTO ir_model (name, model, state, transient) \
         VALUES ('Company Shared', $1, 'base', false) RETURNING id::int8",
        &[&SHARED],
    )
    .await;
    set_rules(
        &client,
        &w,
        shared_model,
        &[("rule 1", Some("[('company_ids', 'in', [-1])]"), true, "r")],
    )
    .await;
    // a model without company field that is denied the same way, by a rule that names no company:
    // the cost reference (reviews RC8, RD10)
    let child = one(
        &client,
        "INSERT INTO x_company_child (name, parent_id) VALUES ('Kid', $1) RETURNING id::int8",
        &[&foreign],
    )
    .await;
    set_rules(
        &client,
        &w,
        w.child_model,
        &[(
            "rule 2",
            // names no company, so the reference does not run the hint path (review RD10)
            Some("[('name', '=', 'x')]"),
            true,
            "r",
        )],
    )
    .await;
    let (orm, source) = policy_orm(&h).await;
    for debug in [true, false] {
        let mut tx = begin_bob(
            &client,
            &orm,
            &source,
            &w,
            &[w.root, w.member],
            &[w.root],
            debug,
        )
        .await;
        let reference = denied_cost(&mut tx, CHILD, child).await;
        assert!(reference >= 1, "the denied read runs statements");
        for (model, id) in [(DOC, foreign), (SHARED, shared)] {
            assert_eq!(
                denied_cost(&mut tx, model, id).await,
                reference,
                "{model}, debug: {debug}"
            );
        }
        drop(tx);
    }
    for debug in [true, false] {
        let mut tx = begin_bob(
            &client,
            &orm,
            &source,
            &w,
            &[w.root, w.member],
            &[w.root],
            debug,
        )
        .await;
        let err = denied(&mut tx, DOC, &[foreign], Operation::Read).await;
        assert!(err.message().ends_with(OUTSIDE), "{err}");
        assert!(!err.message().contains("Company C"), "{err}");
        assert!(!err.message().contains("Foreign record"), "{err}");
        assert!(err.context().is_none());

        // `company_ids`: the first of the subject's companies the record shares
        let err = denied(&mut tx, SHARED, &[shared], Operation::Read).await;
        assert!(err.message().ends_with(&switch_to("Company B")), "{err}");
        assert_eq!(suggested(&err), Some((w.member, "Company B".to_owned())));
        assert!(!err.message().contains("Company C"), "{err}");
        drop(tx);
    }
    let only_foreign = one(
        &client,
        "INSERT INTO x_company_shared (name) VALUES ('Foreign shared') RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            &format!(
                "INSERT INTO \"{}\" (\"{}\", \"{}\") VALUES ($1, $2)",
                rel.table, rel.column1, rel.column2
            ),
            &[&only_foreign, &w.foreign],
        )
        .await
        .unwrap();
    let mut tx = begin_bob(
        &client,
        &orm,
        &source,
        &w,
        &[w.root, w.member],
        &[w.root],
        false,
    )
    .await;
    let err = denied(&mut tx, SHARED, &[only_foreign], Operation::Read).await;
    assert_eq!(
        err.message(),
        format!(
            "{}\n- Company Shared ({SHARED})\n\n{COOKIES}",
            head(&w, "read")
        )
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}
