//! The default company check (4 and 7.1-7.3; design
//! §3, §6.2): what is checked without a declaration, the two opt-outs, the four company matches,
//! the error texts and the statement budget. Own behaviour, so no SPEC tags (the archaeologist
//! assigns CRUD ids after acceptance).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_sdk::{Command, CompanyError, ErrorKind, RawVals, RecId, Records, Transaction, Value};
use erp_testkit::{Harness, assert_query_count};
use support::map;

const HEADER: &str = "Uh-oh! You’ve got some company inconsistencies here:";
const FOOTER: &str = "To avoid a mess, no company crossover is allowed!";

async fn company_orm() -> Harness {
    Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_company::ADDON,
    ])
    .build()
    .await
    .expect("harness")
}

fn rid(i: i64) -> RecId {
    RecId::from_real(i).unwrap()
}

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

fn many(ids: &[i64]) -> Value {
    Value::list(ids.iter().map(|i| Value::Int(*i)))
}

fn text(s: &str) -> Value {
    Value::str(s)
}

/// Companies, targets and users of the scenarios.
struct W {
    /// A root company, a branch of it, and an unrelated company.
    r: i64,
    br: i64,
    o: i64,
    tag_r: i64,
    tag_br: i64,
    tag_o: i64,
    tag_none: i64,
    p_r: i64,
    p_br: i64,
    p_o: i64,
    p_none: i64,
    u_r: i64,
    u_o: i64,
    u_none: i64,
    sh_r: i64,
    sh_br: i64,
    sh_o: i64,
    sh_none: i64,
}

async fn world(tx: &mut Transaction) -> W {
    let c = create(
        tx,
        "res.company",
        &[&[("name", text("Root"))], &[("name", text("Other"))]],
    )
    .await;
    let (r, o) = (c.ids()[0], c.ids()[1]);
    let br = create(
        tx,
        "res.company",
        &[&[("name", text("Branch")), ("parent_id", Value::Int(r))]],
    )
    .await
    .ids()[0];
    let tags = create(
        tx,
        "test_company.tag",
        &[
            &[("name", text("TagR")), ("company_id", Value::Int(r))],
            &[("name", text("TagBr")), ("company_id", Value::Int(br))],
            &[("name", text("TagO")), ("company_id", Value::Int(o))],
            &[("name", text("TagNone"))],
        ],
    )
    .await;
    let partners = create(
        tx,
        "test_company.partner",
        &[
            &[("name", text("PR")), ("company_id", Value::Int(r))],
            &[("name", text("PBr")), ("company_id", Value::Int(br))],
            &[("name", text("PO")), ("company_id", Value::Int(o))],
            &[("name", text("PNone"))],
        ],
    )
    .await;
    let users = create(
        tx,
        "res.users",
        &[
            &[
                ("name", text("UR")),
                ("login", text("ur")),
                ("company_ids", many(&[r])),
            ],
            &[
                ("name", text("UO")),
                ("login", text("uo")),
                ("company_ids", many(&[o])),
            ],
            &[("name", text("UNone")), ("login", text("unone"))],
        ],
    )
    .await;
    let shared = create(
        tx,
        "test_company.shared",
        &[
            &[("name", text("ShR")), ("company_ids", many(&[r]))],
            &[("name", text("ShBr")), ("company_ids", many(&[br]))],
            &[("name", text("ShO")), ("company_ids", many(&[o]))],
            &[("name", text("ShNone"))],
        ],
    )
    .await;
    W {
        r,
        br,
        o,
        tag_r: tags.ids()[0],
        tag_br: tags.ids()[1],
        tag_o: tags.ids()[2],
        tag_none: tags.ids()[3],
        p_r: partners.ids()[0],
        p_br: partners.ids()[1],
        p_o: partners.ids()[2],
        p_none: partners.ids()[3],
        u_r: users.ids()[0],
        u_o: users.ids()[1],
        u_none: users.ids()[2],
        sh_r: shared.ids()[0],
        sh_br: shared.ids()[1],
        sh_o: shared.ids()[2],
        sh_none: shared.ids()[3],
    }
}

/// The error of creating a document with `vals`, rolled back.
async fn refused_create(
    tx: &mut Transaction,
    model: &str,
    vals: &[(&str, Value)],
) -> erp_sdk::Error {
    let records = tx.env().records(model).unwrap();
    let v = map(vals);
    tx.savepoint(async |tx| records.create_values(tx, std::slice::from_ref(&v)).await)
        .await
        .unwrap_err()
}

async fn accepted_create(tx: &mut Transaction, model: &str, vals: &[(&str, Value)]) -> Records {
    let records = tx.env().records(model).unwrap();
    records
        .create_values(tx, &[map(vals)])
        .await
        .unwrap_or_else(|e| panic!("{model} {vals:?}: {e}"))
}

/// The message of one inconsistency of a record of the model whose field has `label`.
fn line(record: &str, companies: &str, label: &str, field: &str, values: &str) -> String {
    format!(
        "- “{record}” belongs to company “{companies}” while “{label}” ({field}: {values}) \
         belongs to another company."
    )
}

fn message(lines: &[String]) -> String {
    format!("{HEADER}\n{}\n{FOOTER}", lines.join("\n"))
}

fn assert_refused(err: &erp_sdk::Error, expected: &str) {
    assert_eq!(err.kind(), ErrorKind::UserError, "{err}");
    assert_eq!(err.message(), expected);
}

/// The defaults, as the registry resolved them.
#[tokio::test]
async fn registry_states_follow_the_defaults() {
    use erp_registry::CheckCompany::{Declared, Default, Off};
    let h = company_orm().await;
    let tx = h.begin().await.unwrap();
    let reg = tx.registry();
    let state = |m: &str, f: &str| reg.model(m).unwrap().field(f).unwrap().check_company;
    for f in [
        "tag_id",
        "tag_ids",
        "partner_id",
        "owner_id",
        "shared_ids",
        "parent_doc_id",
    ] {
        assert_eq!(state("test_company.doc", f), Default, "{f}");
    }
    // company-dependent: declared only until per-company storage (review F6)
    assert_eq!(state("test_company.dep", "pref_tag_id"), Declared);
    assert_eq!(state("test_company.line", "doc_id"), Default);
    for f in [
        "company_id",
        "free_tag_id",
        "plain_id",
        "related_tag_id",
        "line_ids",
        "create_uid",
        "write_uid",
    ] {
        assert_eq!(state("test_company.doc", f), Off, "{f}");
    }
    // the delegated fields belong to the parent
    assert_eq!(state("test_company.child", "tag_id"), Off);
    assert_eq!(state("res.company", "default_tag_id"), Declared);
    let auto = |m: &str| reg.model(m).unwrap().check_company_auto;
    assert!(auto("test_company.doc"));
    assert!(auto("test_company.line"));
    assert!(auto("res.company"), "declared on the company fixture");
    assert!(!auto("test_company.free"), "model opt-out");
    assert!(!auto("test_company.tag"), "nothing to check on the tag");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn many2one_and_many2many_are_checked_without_a_declaration() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    // consistent rows, with and without companies
    for vals in [
        vec![
            ("company_id", Value::Int(w.r)),
            ("tag_id", Value::Int(w.tag_r)),
        ],
        vec![
            ("company_id", Value::Int(w.r)),
            ("tag_id", Value::Int(w.tag_none)),
        ],
        vec![("tag_id", Value::Int(w.tag_none))],
        vec![
            ("company_id", Value::Int(w.o)),
            ("tag_ids", many(&[w.tag_o, w.tag_none])),
        ],
        vec![("name", text("empty"))],
    ] {
        accepted_create(&mut tx, "test_company.doc", &vals).await;
    }
    // a foreign tag
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D0")),
            ("company_id", Value::Int(w.r)),
            ("tag_id", Value::Int(w.tag_o)),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D0", "Root", "Tag", "tag_id", "'TagO'")]),
    );
    // a foreign tag among the many2many targets: every target is listed
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D1")),
            ("company_id", Value::Int(w.r)),
            ("tag_ids", many(&[w.tag_r, w.tag_o])),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D1", "Root", "Tags", "tag_ids", "'TagR', 'TagO'")]),
    );
    // a record without company cannot link a record of a company
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[("name", text("D2")), ("tag_id", Value::Int(w.tag_r))],
    )
    .await;
    assert_refused(&err, &message(&[line("D2", "", "Tag", "tag_id", "'TagR'")]));
    // two fields, in field order
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D3")),
            ("company_id", Value::Int(w.r)),
            ("tag_ids", many(&[w.tag_o])),
            ("tag_id", Value::Int(w.tag_o)),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[
            line("D3", "Root", "Tag", "tag_id", "'TagO'"),
            line("D3", "Root", "Tags", "tag_ids", "'TagO'"),
        ]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn opt_outs_keep_references_behaviour() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    // field opt-out
    accepted_create(
        &mut tx,
        "test_company.doc",
        &[
            ("company_id", Value::Int(w.r)),
            ("free_tag_id", Value::Int(w.tag_o)),
        ],
    )
    .await;
    // model opt-out
    accepted_create(
        &mut tx,
        "test_company.free",
        &[
            ("company_id", Value::Int(w.r)),
            ("tag_id", Value::Int(w.tag_o)),
        ],
    )
    .await;
    // not checked: the target has no company, related, the company field itself, one2many
    accepted_create(
        &mut tx,
        "test_company.doc",
        &[("company_id", Value::Int(w.r))],
    )
    .await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn parent_of_accepts_the_company_and_its_ancestors() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    // the branch may use its own partner, the root's, and one without company
    for p in [w.p_br, w.p_r, w.p_none] {
        accepted_create(
            &mut tx,
            "test_company.doc",
            &[
                ("company_id", Value::Int(w.br)),
                ("partner_id", Value::Int(p)),
            ],
        )
        .await;
    }
    // not an unrelated company's
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D0")),
            ("company_id", Value::Int(w.br)),
            ("partner_id", Value::Int(w.p_o)),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D0", "Branch", "Partner", "partner_id", "'PO'")]),
    );
    // the root may not use its branch's partner
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D1")),
            ("company_id", Value::Int(w.r)),
            ("partner_id", Value::Int(w.p_br)),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D1", "Root", "Partner", "partner_id", "'PBr'")]),
    );
    // a parent match on a write: the company moves to the root, the branch partner is refused
    let doc = accepted_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D2")),
            ("company_id", Value::Int(w.br)),
            ("partner_id", Value::Int(w.p_br)),
        ],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    let err = tx
        .savepoint(async |tx| doc.set_value(tx, "company_id", Value::Int(w.r)).await)
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[line("D2", "Root", "Partner", "partner_id", "'PBr'")]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn any_of_checks_links_to_users_against_their_companies() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    accepted_create(
        &mut tx,
        "test_company.doc",
        &[
            ("company_id", Value::Int(w.r)),
            ("owner_id", Value::Int(w.u_r)),
        ],
    )
    .await;
    // a record without company accepts any user
    accepted_create(
        &mut tx,
        "test_company.doc",
        &[("owner_id", Value::Int(w.u_o))],
    )
    .await;
    accepted_create(
        &mut tx,
        "test_company.doc",
        &[("owner_id", Value::Int(w.u_none))],
    )
    .await;
    // a user of another company, and a user of none
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D0")),
            ("company_id", Value::Int(w.r)),
            ("owner_id", Value::Int(w.u_o)),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D0", "Root", "Owner", "owner_id", "'UO'")]),
    );
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D1")),
            ("company_id", Value::Int(w.r)),
            ("owner_id", Value::Int(w.u_none)),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D1", "Root", "Owner", "owner_id", "'UNone'")]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn parent_of_any_walks_the_ancestors_of_the_shared_companies() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    for (company, shared) in [
        (w.br, vec![w.sh_br]),
        (w.br, vec![w.sh_r]),
        (w.r, vec![w.sh_r]),
    ] {
        accepted_create(
            &mut tx,
            "test_company.doc",
            &[
                ("company_id", Value::Int(company)),
                ("shared_ids", many(&shared)),
            ],
        )
        .await;
    }
    // a record without company accepts any shared record
    accepted_create(
        &mut tx,
        "test_company.doc",
        &[("shared_ids", many(&[w.sh_o]))],
    )
    .await;
    // the root may not use the branch's
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D0")),
            ("company_id", Value::Int(w.r)),
            ("shared_ids", many(&[w.sh_br])),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D0", "Root", "Shared", "shared_ids", "'ShBr'")]),
    );
    // every target must fit: one foreign shared record refuses the field
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D2")),
            ("company_id", Value::Int(w.br)),
            ("shared_ids", many(&[w.sh_r, w.sh_o])),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D2", "Branch", "Shared", "shared_ids", "'ShR', 'ShO'")]),
    );
    // nor one with no company at all
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D1")),
            ("company_id", Value::Int(w.br)),
            ("shared_ids", many(&[w.sh_none])),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D1", "Branch", "Shared", "shared_ids", "'ShNone'")]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn archived_targets_are_still_checked() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let tags = tx
        .env()
        .records("test_company.tag")
        .unwrap()
        .browse_ids([rid(w.tag_o), rid(w.tag_r)]);
    tags.set_value(&mut tx, "active", Value::Bool(false))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D0")),
            ("company_id", Value::Int(w.r)),
            ("tag_ids", many(&[w.tag_r, w.tag_o])),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("D0", "Root", "Tags", "tag_ids", "'TagR', 'TagO'")]),
    );
    // an archived target of the record's own company is fine
    accepted_create(
        &mut tx,
        "test_company.doc",
        &[
            ("company_id", Value::Int(w.r)),
            ("tag_id", Value::Int(w.tag_r)),
        ],
    )
    .await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_target_that_does_not_exist_is_reported() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let doc = accepted_create(
        &mut tx,
        "test_company.doc",
        &[("company_id", Value::Int(w.r))],
    )
    .await;
    let err = tx
        .savepoint(async |tx| doc.set_value(tx, "tag_id", Value::Int(987_654_321)).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        "Record does not exist or has been deleted: test_company.tag,987654321 (linked by \
         test_company.doc.tag_id)"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn x2many_commands_check_the_final_set() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let doc = accepted_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D0")),
            ("company_id", Value::Int(w.r)),
            ("tag_ids", many(&[w.tag_r])),
        ],
    )
    .await;
    let commands = |c: Vec<Command>| RawVals::new().set_commands("tag_ids", c);
    let bad = message(&[line("D0", "Root", "Tags", "tag_ids", "'TagR', 'TagO'")]);
    // link a foreign tag
    let err = tx
        .savepoint(async |tx| {
            doc.write(tx, commands(vec![Command::link(rid(w.tag_o))]))
                .await
        })
        .await
        .unwrap_err();
    assert_refused(&err, &bad);
    // set with a foreign tag
    let err = tx
        .savepoint(async |tx| {
            doc.write(
                tx,
                commands(vec![Command::set([rid(w.tag_r), rid(w.tag_o)])]),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_refused(&err, &bad);
    // create a tag of another company through the command
    let err = tx
        .savepoint(async |tx| {
            let vals = RawVals::new()
                .set_cell("name", erp_sdk::Cell::Str("TagNew".into()))
                .set_cell("company_id", erp_sdk::Cell::Id(w.o));
            doc.write(tx, commands(vec![Command::create(vals)])).await
        })
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[line("D0", "Root", "Tags", "tag_ids", "'TagR', 'TagNew'")]),
    );
    // a foreign tag that is unlinked again in the same list is not part of the final set
    doc.write(
        &mut tx,
        commands(vec![
            Command::link(rid(w.tag_o)),
            Command::unlink(rid(w.tag_o)),
        ]),
    )
    .await
    .unwrap();
    // clear and set to the company's own and a shared one
    doc.write(&mut tx, commands(vec![Command::clear()]))
        .await
        .unwrap();
    doc.write(
        &mut tx,
        commands(vec![Command::set([rid(w.tag_none), rid(w.tag_r)])]),
    )
    .await
    .unwrap();
    // an unlink of the offending tag repairs the record
    let doc2 = accepted_create(
        &mut tx,
        "test_company.doc",
        &[
            ("company_id", Value::Int(w.r)),
            ("tag_ids", many(&[w.tag_r])),
        ],
    )
    .await;
    doc2.write(&mut tx, commands(vec![Command::unlink(rid(w.tag_r))]))
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn writing_the_company_rechecks_every_checked_field() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let doc = accepted_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D0")),
            ("company_id", Value::Int(w.r)),
            ("tag_id", Value::Int(w.tag_r)),
            ("tag_ids", many(&[w.tag_r])),
            ("partner_id", Value::Int(w.p_r)),
        ],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    // other company: tag_id, tag_ids; the partner of the root is not the other company's
    let err = tx
        .savepoint(async |tx| doc.set_value(tx, "company_id", Value::Int(w.o)).await)
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[
            line("D0", "Other", "Tag", "tag_id", "'TagR'"),
            line("D0", "Other", "Tags", "tag_ids", "'TagR'"),
            line("D0", "Other", "Partner", "partner_id", "'PR'"),
        ]),
    );
    // no company: the same
    let err = tx
        .savepoint(async |tx| doc.set_value(tx, "company_id", Value::False).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    // writing an unrelated field checks nothing: no statement besides the flush
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, <= 0, {
        doc.set_value(&mut tx, "name", text("renamed")).await.unwrap();
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn lines_check_their_inverse_and_the_one2many_is_not_checked() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let doc = accepted_create(
        &mut tx,
        "test_company.doc",
        &[("name", text("D0")), ("company_id", Value::Int(w.r))],
    )
    .await;
    let line_vals = |company: i64| {
        RawVals::new()
            .set_cell("name", erp_sdk::Cell::Str("L0".into()))
            .set_cell("company_id", erp_sdk::Cell::Id(company))
    };
    doc.write(
        &mut tx,
        RawVals::new().set_commands("line_ids", [Command::create(line_vals(w.r))]),
    )
    .await
    .unwrap();
    let err = tx
        .savepoint(async |tx| {
            doc.write(
                tx,
                RawVals::new().set_commands("line_ids", [Command::create(line_vals(w.o))]),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[line("L0", "Other", "Document", "doc_id", "'D0'")]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_record_may_not_link_a_parent_of_another_company() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let parent = accepted_create(
        &mut tx,
        "test_company.doc",
        &[("name", text("Parent")), ("company_id", Value::Int(w.o))],
    )
    .await;
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("Child")),
            ("company_id", Value::Int(w.r)),
            ("parent_doc_id", Value::Int(parent.ids()[0])),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line(
            "Child",
            "Root",
            "Parent Document",
            "parent_doc_id",
            "'Parent'",
        )]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn delegated_fields_are_checked_on_the_parent_record() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    accepted_create(
        &mut tx,
        "test_company.child",
        &[
            ("name", text("C0")),
            ("company_id", Value::Int(w.r)),
            ("tag_id", Value::Int(w.tag_r)),
        ],
    )
    .await;
    let err = refused_create(
        &mut tx,
        "test_company.child",
        &[
            ("name", text("C1")),
            ("company_id", Value::Int(w.r)),
            ("tag_id", Value::Int(w.tag_o)),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("C1", "Root", "Tag", "tag_id", "'TagO'")]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// At most five inconsistencies are listed; the error counts all of them.
#[tokio::test]
async fn only_five_inconsistencies_are_listed_and_all_are_counted() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    for (bad, listed) in [(6usize, 5usize), (7, 5), (5, 5), (2, 2)] {
        let docs = tx.env().records("test_company.doc").unwrap();
        let rows: Vec<_> = (0..bad)
            .map(|i| {
                map(&[
                    ("name", text(&format!("D{i}"))),
                    ("company_id", Value::Int(w.r)),
                    ("tag_id", Value::Int(w.tag_o)),
                ])
            })
            .collect();
        let err = tx
            .savepoint(async |tx| docs.create_values(tx, &rows).await)
            .await
            .unwrap_err();
        let lines: Vec<&str> = err.message().lines().collect();
        assert_eq!(lines.len(), listed + 2, "{bad}: {lines:?}");
        assert_eq!(lines[0], HEADER);
        assert_eq!(lines[1], line("D0", "Root", "Tag", "tag_id", "'TagO'"));
        assert_eq!(lines[listed + 1], FOOTER);
        let source = std::error::Error::source(&err)
            .and_then(|s| s.downcast_ref::<CompanyError>())
            .expect("the CompanyError is the source");
        match source {
            CompanyError::Inconsistent { items, total } => {
                assert_eq!(*total, bad);
                // review T2: the listed items are data, the message is rendered from them
                assert_eq!(items.len(), listed);
                assert_eq!(
                    (
                        items[0].record.as_str(),
                        items[0].field,
                        items[0].values.as_slice()
                    ),
                    ("D0", "tag_id", ["'TagO'".to_owned()].as_slice())
                );
                assert_eq!(items[0].companies, ["Root".to_owned()]);
                assert_eq!(source.to_string(), err.message());
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Names are shown as Python reprs: quotes, non-ASCII and line breaks.
#[tokio::test]
async fn names_are_shown_as_python_reprs() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let foreign = create(
        &mut tx,
        "test_company.tag",
        &[
            &[("name", text("O'Neil")), ("company_id", Value::Int(w.o))],
            &[
                ("name", text("say \"hi\"")),
                ("company_id", Value::Int(w.o)),
            ],
            &[
                ("name", text("it's \"x\"")),
                ("company_id", Value::Int(w.o)),
            ],
            &[
                ("name", text("Société d’Ébène 標籤")),
                ("company_id", Value::Int(w.o)),
            ],
            &[("name", text("a\r\nb")), ("company_id", Value::Int(w.o))],
        ],
    )
    .await;
    let err = refused_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D’Ébène \"q\"")),
            ("company_id", Value::Int(w.r)),
            ("tag_ids", many(&foreign.ids())),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line(
            "D’Ébène \"q\"",
            "Root",
            "Tags",
            "tag_ids",
            "\"O'Neil\", 'say \"hi\"', 'it\\'s \"x\"', 'Société d’Ébène 標籤', 'a\\r\\nb'",
        )]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The check runs under every elevation: `sudo()` does not skip it.
#[tokio::test]
async fn the_check_runs_under_sudo() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let docs = tx.env().sudo(true).records("test_company.doc").unwrap();
    let v = map(&[
        ("name", text("D0")),
        ("company_id", Value::Int(w.r)),
        ("tag_id", Value::Int(w.tag_o)),
    ]);
    let err = tx
        .savepoint(async |tx| docs.create_values(tx, std::slice::from_ref(&v)).await)
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[line("D0", "Root", "Tag", "tag_id", "'TagO'")]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A `res.company` record is its own company (CRUD-061 form a) on a declared field.
#[tokio::test]
async fn a_company_checks_its_declared_fields_against_itself() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    accepted_create(
        &mut tx,
        "res.company",
        &[
            ("name", text("Fine")),
            ("default_tag_id", Value::Int(w.tag_none)),
        ],
    )
    .await;
    let err = refused_create(
        &mut tx,
        "res.company",
        &[
            ("name", text("Self")),
            ("default_tag_id", Value::Int(w.tag_o)),
        ],
    )
    .await;
    // the company is created before the check: its name is "Self" and it is its own company
    assert_refused(
        &err,
        &format!(
            "{HEADER}\n- Record is company “Self” while “Default Tag” (default_tag_id: 'TagO') \
             belongs to another company.\n{FOOTER}"
        ),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The cost: one statement for a batch of 1,000 rows over several checked fields, none when the
/// cache has everything.
#[tokio::test]
async fn a_batch_costs_one_statement_and_a_warm_cache_none() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let docs = tx.env().records("test_company.doc").unwrap();
    let rows: Vec<_> = (0..1000)
        .map(|i| {
            map(&[
                ("name", text(&format!("D{i}"))),
                ("company_id", Value::Int(w.br)),
                ("tag_id", Value::Int(w.tag_br)),
                ("tag_ids", many(&[w.tag_br, w.tag_none])),
                ("partner_id", Value::Int(w.p_r)),
            ])
        })
        .collect();
    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    let created = docs.create_values(&mut tx, &rows).await.unwrap();
    assert_eq!(created.len(), 1000);
    // every statement counts (review D3): the check is the one that is not the write itself, so
    // no settle, prefetch or second read hides behind the marker
    let others: Vec<&String> = tx
        .captured()
        .iter()
        .filter(|sql| !sql.contains("NULL::varchar"))
        .collect();
    // the writes themselves: the row insert and the relation rows of `tag_ids`
    assert_eq!(
        others.len(),
        2,
        "only the writes besides the check: {others:#?}"
    );
    assert!(others[0].contains("unnest($1::int8[]") && others[1].starts_with("INSERT INTO"));
    let checks = tx.captured().len() - others.len();
    assert_eq!(checks, 1, "{:#?}", tx.captured());
    tx.capture_statements(false);
    // the same values again: the targets' companies are cached now
    let more: Vec<_> = rows.iter().take(10).cloned().collect();
    tx.capture_statements(true);
    docs.create_values(&mut tx, &more).await.unwrap();
    assert_eq!(
        tx.captured().len(),
        2,
        "a warm cache reads nothing, only the two writes run: {:#?}",
        tx.captured()
    );
    tx.capture_statements(false);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A huge batch splits into statements of at most the chunk size (lowered by the test-faults
/// transaction setter): seven distinct ids in chunks of three read in three statements, and the
/// decision still sees every chunk.
#[tokio::test]
async fn a_batch_over_the_chunk_runs_one_statement_per_chunk() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let doc = accepted_create(
        &mut tx,
        "test_company.doc",
        &[("name", text("D0")), ("company_id", Value::Int(w.r))],
    )
    .await
    .ids()[0];
    let rows: Vec<_> = (0..7)
        .map(|i| {
            map(&[
                ("name", text(&format!("L{i}"))),
                ("company_id", Value::Int(w.r)),
                ("doc_id", Value::Int(doc)),
            ])
        })
        .collect();
    let lines = tx
        .env()
        .records("test_company.line")
        .unwrap()
        .create_values(&mut tx, &rows)
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    // the old context key is just a context value now: a caller cannot lower the chunk with it
    // (review R10), the seven ids are read in one statement
    let by_context = tx
        .env()
        .with_context([("company_check_chunk_for_tests", Value::Int(3))])
        .records("test_company.line")
        .unwrap()
        .browse_ids(lines.rec_ids().iter().copied());
    tx.capture_statements(true);
    tx.savepoint(async |tx| {
        by_context
            .set_value(tx, "company_id", Value::Int(w.o))
            .await
    })
    .await
    .unwrap_err();
    let context_reads = tx
        .captured()
        .iter()
        .filter(|s| s.contains("NULL::varchar"))
        .count();
    assert_eq!(context_reads, 1, "{:#?}", tx.captured());
    tx.capture_statements(false);
    tx.invalidate_all().await.unwrap();
    // writing the company re-checks doc_id of seven lines: seven distinct ids to read
    tx.set_company_check_chunk_for_tests(3).unwrap();
    let chunked = tx
        .env()
        .records("test_company.line")
        .unwrap()
        .browse_ids(lines.rec_ids().iter().copied());
    tx.capture_statements(true);
    let err = chunked
        .set_value(&mut tx, "company_id", Value::Int(w.o))
        .await
        .unwrap_err();
    let reads = tx
        .captured()
        .iter()
        .filter(|s| s.contains("NULL::varchar"))
        .count();
    assert_eq!(reads, 3, "{:#?}", tx.captured());
    tx.capture_statements(false);
    let source = std::error::Error::source(&err)
        .and_then(|s| s.downcast_ref::<CompanyError>())
        .expect("the CompanyError is the source");
    match source {
        CompanyError::Inconsistent { total, .. } => assert_eq!(*total, 7),
        other => panic!("unexpected {other:?}"),
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A company-dependent field is checked against the current company, not the record's. The values
/// stay in the cache (no flush): per-company storage of such fields arrives with, until
/// then their jsonb column cannot take an id.
#[tokio::test]
async fn company_dependent_fields_are_checked_against_the_current_company() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let in_root = tx.env().with_company(w.r).unwrap();
    let dep = accepted_create(
        &mut tx,
        "test_company.dep",
        &[("company_id", Value::Int(w.o))],
    )
    .await
    .ids()[0];
    let with_company = in_root
        .records("test_company.dep")
        .unwrap()
        .browse_ids([rid(dep)]);
    let without_company = tx
        .env()
        .records("test_company.dep")
        .unwrap()
        .browse_ids([rid(dep)]);
    // the record is in Other; the preferred tag is checked against Root
    let err = tx
        .savepoint(async |tx| {
            with_company
                .set_value(tx, "pref_tag_id", Value::Int(w.tag_o))
                .await
        })
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[line(
            &format!("test_company.dep,{dep}"),
            // the record's company, not the current one the value is matched against (review R4)
            "Other",
            "Preferred Tag",
            "pref_tag_id",
            "'TagO'",
        )]),
    );
    // without a current company a value cannot be checked
    let err = tx
        .savepoint(async |tx| {
            without_company
                .set_value(tx, "pref_tag_id", Value::Int(w.tag_r))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "test_company.dep.pref_tag_id is company-dependent and was written without a current \
         company"
    );
    // an empty value needs no company; a value of the current company is consistent
    without_company
        .set_value(&mut tx, "pref_tag_id", Value::False)
        .await
        .unwrap();
    with_company
        .set_value(&mut tx, "pref_tag_id", Value::Int(w.tag_r))
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Names are read as the caller: a record the caller cannot read appears as
/// `<model>(<id>,)`, so the error does not leak names; also under `sudo()` (review S3).
#[tokio::test]
async fn names_the_caller_cannot_read_are_not_leaked() {
    use erp_sdk::Operation;
    use erp_testkit::{DomainPolicy, Otherwise};
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .deny("test_company.tag", Operation::Read)
        .build()
        .unwrap();
    let h = Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_company::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .expect("harness");
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    tx.commit().await.unwrap();
    let mut tx = h.begin_as(w.u_r).await.unwrap();
    let docs = tx.env().records("test_company.doc").unwrap();
    let v = map(&[
        ("name", text("D0")),
        ("company_id", Value::Int(w.r)),
        ("tag_id", Value::Int(w.tag_o)),
    ]);
    let err = tx
        .savepoint(async |tx| docs.create_values(tx, std::slice::from_ref(&v)).await)
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[line(
            "D0",
            "Root",
            "Tag",
            "tag_id",
            &format!("test_company.tag({},)", w.tag_o),
        )]),
    );
    // elevated, the names are still read as the request principal (review S3): nothing leaks
    let sudo_docs = tx.env().sudo(true).records("test_company.doc").unwrap();
    let err = tx
        .savepoint(async |tx| sudo_docs.create_values(tx, std::slice::from_ref(&v)).await)
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[line(
            "D0",
            "Root",
            "Tag",
            "tag_id",
            &format!("test_company.tag({},)", w.tag_o),
        )]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// An empty recordset and duplicate ids cost nothing and break nothing.
#[tokio::test]
async fn empty_sets_and_duplicate_ids() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let doc = accepted_create(
        &mut tx,
        "test_company.doc",
        &[("name", text("D0")), ("company_id", Value::Int(w.r))],
    )
    .await
    .ids()[0];
    let docs = tx.env().records("test_company.doc").unwrap();
    let none = docs.browse_ids([]);
    assert_query_count!(tx, <= 0, {
        none.set_value(&mut tx, "tag_id", Value::Int(w.tag_o)).await.unwrap();
    });
    // the same record twice: one inconsistency, not two
    let twice = docs.browse_ids([rid(doc), rid(doc)]);
    let err = tx
        .savepoint(async |tx| twice.set_value(tx, "tag_id", Value::Int(w.tag_o)).await)
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[line("D0", "Root", "Tag", "tag_id", "'TagO'")]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review D1: a stored compute that is a checked field is settled before it is read. On create
/// the pending value was read as NULL and never checked; fails on the old code (the create is
/// accepted, or panics on the debug guard).
#[tokio::test]
async fn a_stored_computed_checked_field_is_checked_on_create() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let err = refused_create(
        &mut tx,
        "test_company.calc",
        &[
            ("name", text("C0")),
            ("company_id", Value::Int(w.r)),
            ("source_id", Value::Int(w.tag_o)),
        ],
    )
    .await;
    assert_refused(
        &err,
        &message(&[line("C0", "Root", "Computed Tag", "tag_id", "'TagO'")]),
    );
    accepted_create(
        &mut tx,
        "test_company.calc",
        &[
            ("name", text("C1")),
            ("company_id", Value::Int(w.r)),
            ("source_id", Value::Int(w.tag_r)),
        ],
    )
    .await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review D1: on a write of the company the recomputed value of a stored compute is checked, not
/// the stale cached one; fails on the old code (the stale `TagR` is consistent, the write passes).
#[tokio::test]
async fn a_stored_computed_checked_field_is_checked_after_its_inputs_change() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let calc = accepted_create(
        &mut tx,
        "test_company.calc",
        &[
            ("name", text("C0")),
            ("company_id", Value::Int(w.r)),
            ("source_id", Value::Int(w.tag_r)),
        ],
    )
    .await;
    let err = tx
        .savepoint(async |tx| {
            let vals = map(&[
                ("source_id", Value::Int(w.tag_o)),
                ("company_id", Value::Int(w.r)),
            ]);
            calc.write_values(tx, &vals).await
        })
        .await
        .unwrap_err();
    assert_refused(
        &err,
        &message(&[line("C0", "Root", "Computed Tag", "tag_id", "'TagO'")]),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The statement of the check, found among the captured ones.
fn check_statement(tx: &Transaction) -> String {
    let found: Vec<&String> = tx
        .captured()
        .iter()
        .filter(|sql| sql.contains("NULL::varchar"))
        .collect();
    assert_eq!(found.len(), 1, "{:#?}", tx.captured());
    found[0].clone()
}

/// The plan of `sql` with every `$n` bound to `ids` (the shape does not depend on which ids).
async fn plan_of(tx: &mut Transaction, sql: &str, ids: &[i64]) -> String {
    let highest = (1..=32)
        .filter(|n| sql.contains(&format!("${n}::")))
        .max()
        .unwrap();
    let owned: Vec<Vec<i64>> = (0..highest).map(|_| ids.to_vec()).collect();
    let refs: Vec<&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)> = owned
        .iter()
        .map(|p| p as &(dyn erp_testkit::tokio_postgres::types::ToSql + Sync))
        .collect();
    let rows = tx
        .query(&format!("EXPLAIN (COSTS OFF) {sql}"), &refs)
        .await
        .unwrap();
    rows.iter()
        .map(|r| r.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Review D2: the target branches are one `UNION ALL` branch per condition, so each can use the
/// primary key of a big comodel. Fails on the old code: `id = ANY($n) OR id IN (SELECT …)` cannot
/// use an index and scans the whole comodel.
#[tokio::test]
async fn the_check_reads_a_big_comodel_through_its_index() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    tx.execute(
        "INSERT INTO test_company_tag (name, company_id, active) \
         SELECT 'bulk' || g, $1, true FROM generate_series(1, 20000) g",
        &[&w.r],
    )
    .await
    .unwrap();
    let docs = create(
        &mut tx,
        "test_company.doc",
        &[
            &[
                ("name", text("D0")),
                ("company_id", Value::Int(w.r)),
                ("tag_id", Value::Int(w.tag_r)),
            ],
            &[
                ("name", text("D1")),
                ("company_id", Value::Int(w.r)),
                ("tag_id", Value::Int(w.tag_r)),
            ],
        ],
    )
    .await;
    tx.flush().await.unwrap();
    tx.execute("ANALYZE test_company_tag", &[]).await.unwrap();
    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    docs.set_value(&mut tx, "company_id", Value::Int(w.r))
        .await
        .unwrap();
    let sql = check_statement(&tx);
    tx.capture_statements(false);
    assert!(!sql.contains(" OR "), "{sql}");
    let plan = plan_of(&mut tx, &sql, &docs.ids()).await;
    assert!(!plan.contains("Seq Scan on test_company_tag"), "{plan}");
    assert!(plan.contains("test_company_tag_pkey"), "{plan}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review R1: the companies of a `company_ids` target and the link rows of a many2many are raw
/// reads (archived targets included), so they are read again by every check that needs them: a
/// warm check of such a field costs exactly one read statement, a warm check of a many2one
/// company or of cached parent paths none. The cost is documented.
#[tokio::test]
async fn a_warm_check_of_company_ids_targets_costs_one_read() {
    let h = company_orm().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx).await;
    let doc = accepted_create(
        &mut tx,
        "test_company.doc",
        &[
            ("name", text("D0")),
            ("company_id", Value::Int(w.r)),
            ("owner_id", Value::Int(w.u_r)),
            ("shared_ids", many(&[w.sh_r])),
        ],
    )
    .await;
    let set_owner = |owner: i64| RawVals::new().set_cell("owner_id", erp_sdk::Cell::Id(owner));
    let set_shared = |ids: &[i64]| {
        RawVals::new().set_commands(
            "shared_ids",
            vec![Command::set(ids.iter().map(|i| rid(*i)))],
        )
    };
    let other_user = create(
        &mut tx,
        "res.users",
        &[&[
            ("name", text("UR2")),
            ("login", text("ur2")),
            ("company_ids", many(&[w.r])),
        ]],
    )
    .await
    .ids()[0];
    let other_shared = create(
        &mut tx,
        "test_company.shared",
        &[&[("name", text("ShR2")), ("company_ids", many(&[w.r]))]],
    )
    .await
    .ids()[0];
    // nothing about the targets is cached: the first checks read them, the repeats are warm
    tx.invalidate_all().await.unwrap();
    doc.write(&mut tx, set_owner(w.u_r)).await.unwrap();
    doc.write(&mut tx, set_owner(other_user)).await.unwrap();
    doc.write(&mut tx, set_shared(&[w.sh_r])).await.unwrap();
    doc.write(&mut tx, set_shared(&[other_shared]))
        .await
        .unwrap();
    // warm: the same targets were checked above; the raw `company_ids` reads still run once each
    tx.capture_statements(true);
    doc.write(&mut tx, set_owner(w.u_r)).await.unwrap();
    let owner_reads = tx
        .captured()
        .iter()
        .filter(|sql| sql.starts_with("SELECT"))
        .count();
    tx.capture_statements(false);
    tx.capture_statements(true);
    doc.write(&mut tx, set_shared(&[w.sh_r])).await.unwrap();
    let shared_reads = tx
        .captured()
        .iter()
        .filter(|sql| sql.starts_with("SELECT"))
        .count();
    let all = tx.captured().to_vec();
    tx.capture_statements(false);
    assert_eq!((owner_reads, shared_reads), (1, 1), "{all:#?}");
    drop(tx);
    h.cleanup().await.unwrap();
}
