//! Company-bound related reads (design, §6.3):
//! a related field that does not declare `compute_sudo` is read as superuser for permissions, but
//! every record reached through a company-scoped hop must belong to the reader's active
//! companies (or to none). Own behaviour, so no SPEC tags (the case/070 tests stay
//! in `access_domains.rs`). Every compared value comes from our code.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::num::NonZeroU64;

use erp_orm::{
    BeginOptions, CompanyId, Context, GroupSet, Principal, PrincipalBuilder, Resolution, UserId,
};
use erp_sdk::{Cell, ErrorKind, Records, Transaction, Value};
use erp_test_models::{base_users, test_company, test_company_related};
use erp_testkit::{DomainPolicy, Harness, Otherwise, assert_query_count};
use support::map;

use erp_sdk::domain::Value as DV;

const DOC: &str = "test_company_related.doc";
const MID: &str = "test_company_related.mid";
const LEAF: &str = "test_company_related.leaf";
const KID: &str = "test_company_related.kid";
const HOLDER: &str = "test_company_related.holder";
const HOLDER1: &str = "test_company_related.holder1";
const TAG: &str = "test_company.tag";
const DEMO: i64 = 2;

const OUTSIDE: &str = "This seems to be a multi-company issue, but you do not have access to the \
                       proper company to access the record anyhow.";

async fn harness() -> Harness {
    let policy = DomainPolicy::builder(Otherwise::Allow).build().unwrap();
    Harness::builder([
        &base_users::ADDON,
        &test_company::ADDON,
        &test_company_related::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .expect("harness")
}

fn company(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

fn ids(list: &[i64]) -> Value {
    Value::list(list.iter().map(|i| Value::Int(*i)))
}

/// A user principal of `uid` who belongs to `member_of` (the first is the main company).
fn principal(uid: i64, member_of: &[i64]) -> Principal {
    let resolution = Resolution::in_id_order(
        GroupSet::default(),
        member_of.iter().map(|c| company(*c)),
        NonZeroU64::MIN,
    )
    .unwrap()
    .with_default_order(member_of.iter().map(|c| company(*c)).collect())
    .unwrap();
    PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(resolution)
        .companies(member_of.iter().map(|c| company(*c)).collect())
        .build()
        .unwrap()
}

/// A transaction of a user in `member_of` who works in `active`.
async fn begin(h: &Harness, member_of: &[i64], active: &[i64]) -> Transaction {
    let context = Context::from_pairs([("allowed_company_ids", ids(active))]);
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal(DEMO, member_of)).context(context))
        .await
        .unwrap()
}

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

fn text(s: &str) -> Value {
    Value::str(s)
}

fn int(i: i64) -> Value {
    Value::Int(i)
}

/// Companies and documents: every document reads a leaf through a mid.
struct W {
    /// A root company, a branch of it, and an unrelated company.
    r: i64,
    br: i64,
    o: i64,
    /// Documents: mid and leaf both in R / mid and leaf without company / mid in O /
    /// mid in R with a leaf in O.
    own: i64,
    free: i64,
    mid_foreign: i64,
    leaf_foreign: i64,
    mid_o: i64,
    mid_r: i64,
    leaf_r: i64,
    leaf_o: i64,
}

async fn world(h: &Harness) -> W {
    let mut tx = h.begin().await.unwrap();
    let c = create(
        &mut tx,
        "res.company",
        &[&[("name", text("Root"))], &[("name", text("Other"))]],
    )
    .await;
    let (r, o) = (c.ids()[0], c.ids()[1]);
    let br = create(
        &mut tx,
        "res.company",
        &[&[("name", text("Branch")), ("parent_id", int(r))]],
    )
    .await
    .ids()[0];
    let leaves = create(
        &mut tx,
        LEAF,
        &[
            &[("name", text("LeafR")), ("company_id", int(r))],
            &[("name", text("LeafNone"))],
            &[("name", text("LeafO")), ("company_id", int(o))],
        ],
    )
    .await;
    let (leaf_r, leaf_none, leaf_o) = (leaves.ids()[0], leaves.ids()[1], leaves.ids()[2]);
    let mids = create(
        &mut tx,
        MID,
        &[
            &[
                ("name", text("MidR")),
                ("company_id", int(r)),
                ("leaf_id", int(leaf_r)),
            ],
            &[("name", text("MidNone")), ("leaf_id", int(leaf_none))],
            &[
                ("name", text("MidO")),
                ("company_id", int(o)),
                ("leaf_id", int(leaf_r)),
            ],
            &[
                ("name", text("MidRLeafO")),
                ("company_id", int(r)),
                ("leaf_id", int(leaf_o)),
            ],
        ],
    )
    .await;
    let docs = create(
        &mut tx,
        DOC,
        &[
            &[("name", text("own")), ("mid_id", int(mids.ids()[0]))],
            &[("name", text("free")), ("mid_id", int(mids.ids()[1]))],
            &[
                ("name", text("mid_foreign")),
                ("mid_id", int(mids.ids()[2])),
            ],
            &[
                ("name", text("leaf_foreign")),
                ("mid_id", int(mids.ids()[3])),
            ],
        ],
    )
    .await;
    // the user of the scenarios exists, so what he creates can name him as its author
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    W {
        r,
        br,
        o,
        own: docs.ids()[0],
        free: docs.ids()[1],
        mid_foreign: docs.ids()[2],
        leaf_foreign: docs.ids()[3],
        mid_o: mids.ids()[2],
        mid_r: mids.ids()[0],
        leaf_r,
        leaf_o,
    }
}

fn doc(tx: &Transaction, id: i64) -> Records {
    tx.env().records(DOC).unwrap().browse([id]).unwrap()
}

/// The text a cell holds.
fn string(cell: &Cell) -> &str {
    match cell {
        Cell::Str(s) => s,
        other => panic!("not a string: {other:?}"),
    }
}

async fn read(tx: &mut Transaction, id: i64, field: &str) -> erp_sdk::Result<Cell> {
    let cells = doc(tx, id).cells(tx, field).await?;
    Ok(cells.into_iter().next().unwrap())
}

fn hop_message(
    field_label: &str,
    field: &str,
    doc_id: i64,
    hop_description: &str,
    hop_model: &str,
    hop_id: Option<i64>,
    hint: &str,
) -> String {
    hop_message_on(
        (DOC, "Related Doc"),
        field_label,
        field,
        doc_id,
        (hop_description, hop_model, hop_id),
        hint,
    )
}

/// [`hop_message`] for a field of another model: `(name, description)` of the record's model,
/// `(description, name, id)` of the hop.
fn hop_message_on(
    (model, description): (&str, &str),
    field_label: &str,
    field: &str,
    record: i64,
    (hop_description, hop_model, hop_id): (&str, &str, Option<i64>),
    hint: &str,
) -> String {
    // the id of a hop reached through another hop is not shown (review SC6)
    let hop = match hop_id {
        Some(id) => format!("{hop_model}: {id}"),
        None => hop_model.to_owned(),
    };
    format!(
        "You cannot read “{field_label}” ({model}.{field}) of {description} ({model}: {record}): \
         it is read through {hop_description} ({hop}), which belongs to a company \
         outside your active companies.\n\n{hint}"
    )
}

const SWITCH_TO_OTHER: &str = "This seems to be a multi-company issue, you might be able to \
                               access the record by switching to the company: Other.";

/// A foreign hop of the path is refused, naming the field, the record and the hop; the first
/// failing hop in path order is the one reported; a company the user is in is suggested, one
/// the user is not in is never named.
#[tokio::test]
async fn a_foreign_hop_is_refused_with_the_full_message() {
    let h = harness().await;
    let w = world(&h).await;

    // the user is only in R: the hop's company O is not theirs, so it is not named
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let err = read(&mut tx, w.mid_foreign, "leaf_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Leaf Name",
            "leaf_name",
            w.mid_foreign,
            "Related Mid",
            MID,
            Some(w.mid_o),
            OUTSIDE
        )
    );
    assert!(err.context().is_none());
    // the first hop in path order: the mid of `mid_foreign` fails before its leaf is looked at
    let err = read(&mut tx, w.mid_foreign, "mid_name").await.unwrap_err();
    assert_eq!(
        err.message(),
        hop_message(
            "Mid Name",
            "mid_name",
            w.mid_foreign,
            "Related Mid",
            MID,
            Some(w.mid_o),
            OUTSIDE
        )
    );
    // the second hop fails when the first passes
    let err = read(&mut tx, w.leaf_foreign, "leaf_name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Leaf Name",
            "leaf_name",
            w.leaf_foreign,
            "Related Leaf",
            LEAF,
            None,
            OUTSIDE
        )
    );
    // a many2one related field is bound the same way: its hops, not the record it ends on
    let err = read(&mut tx, w.mid_foreign, "leaf_id").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Leaf",
            "leaf_id",
            w.mid_foreign,
            "Related Mid",
            MID,
            Some(w.mid_o),
            OUTSIDE
        )
    );
    // the record a many2one path ends on is one more hop (review SC4): the foreign leaf is
    // refused, without its id
    let err = read(&mut tx, w.leaf_foreign, "leaf_id").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Leaf",
            "leaf_id",
            w.leaf_foreign,
            "Related Leaf",
            LEAF,
            None,
            OUTSIDE
        )
    );
    drop(tx);

    // a member of O who works in R is told which company to switch to
    let mut tx = begin(&h, &[w.r, w.o], &[w.r]).await;
    let err = read(&mut tx, w.mid_foreign, "mid_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    let hint = "This seems to be a multi-company issue, you might be able to access the record \
                by switching to the company: Other.";
    assert_eq!(
        err.message(),
        hop_message(
            "Mid Name",
            "mid_name",
            w.mid_foreign,
            "Related Mid",
            MID,
            Some(w.mid_o),
            hint
        )
    );
    let context = err.context().expect("suggested company");
    let Value::Map(outer) = context else {
        panic!("not a map: {context:?}");
    };
    let Some(Value::Map(suggested)) = outer.get("suggested_company") else {
        panic!("no suggested_company in {context:?}");
    };
    assert_eq!(suggested.get("id"), Some(&int(w.o)));
    assert_eq!(suggested.get("display_name"), Some(&text("Other")));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Records of the active companies and records without company read through; widening the
/// active companies lets the foreign hop through.
#[tokio::test]
async fn own_and_company_less_hops_read() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r, w.o], &[w.r]).await;
    assert_eq!(
        string(&read(&mut tx, w.own, "leaf_name").await.unwrap()),
        "LeafR"
    );
    assert_eq!(
        string(&read(&mut tx, w.free, "leaf_name").await.unwrap()),
        "LeafNone"
    );
    assert_eq!(
        string(&read(&mut tx, w.free, "mid_name").await.unwrap()),
        "MidNone"
    );
    drop(tx);

    // working in both companies, the foreign hop is an active one
    let mut tx = begin(&h, &[w.r, w.o], &[w.r, w.o]).await;
    assert_eq!(
        string(&read(&mut tx, w.mid_foreign, "mid_name").await.unwrap()),
        "MidO"
    );
    assert_eq!(
        string(&read(&mut tx, w.leaf_foreign, "leaf_name").await.unwrap()),
        "LeafO"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn sudo_and_the_declared_compute_sudo_read_through_foreign_hops() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let err = read(&mut tx, w.leaf_foreign, "leaf_name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Leaf Name",
            "leaf_name",
            w.leaf_foreign,
            "Related Leaf",
            LEAF,
            None,
            OUTSIDE
        )
    );
    for field in ["leaf_name_sudo", "leaf_name_user"] {
        assert_eq!(
            string(&read(&mut tx, w.leaf_foreign, field).await.unwrap()),
            "LeafO",
            "{field}"
        );
    }
    let su = doc(&tx, w.leaf_foreign).sudo(true);
    let cells = su.cells(&mut tx, "leaf_name").await.unwrap();
    assert_eq!(string(&cells[0]), "LeafO");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The refusal of `mid_name` on `mid_foreign` for a member of R and O who works in R.
fn mid_foreign_message(w: &W) -> String {
    hop_message(
        "Mid Name",
        "mid_name",
        w.mid_foreign,
        "Related Mid",
        MID,
        Some(w.mid_o),
        SWITCH_TO_OTHER,
    )
}

/// A value computed for one environment is not served to another: a `sudo()` read first, then
/// the bound read in the same transaction still fails; another set of active companies re-reads.
#[tokio::test]
async fn cached_values_do_not_cross_company_contexts() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r, w.o], &[w.r]).await;
    let su = doc(&tx, w.mid_foreign).sudo(true);
    assert_eq!(
        string(&su.cells(&mut tx, "mid_name").await.unwrap()[0]),
        "MidO"
    );
    let err = read(&mut tx, w.mid_foreign, "mid_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(err.message(), mid_foreign_message(&w));

    // the same records in an environment that works in O too: the value is readable there
    let wide = tx
        .env()
        .with_context([("allowed_company_ids", ids(&[w.r, w.o]))]);
    let records = wide.records(DOC).unwrap().browse([w.mid_foreign]).unwrap();
    assert_eq!(
        string(&records.cells(&mut tx, "mid_name").await.unwrap()[0]),
        "MidO"
    );
    // and the narrow environment is still refused afterwards
    let err = read(&mut tx, w.mid_foreign, "mid_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(err.message(), mid_foreign_message(&w));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A 2-hop read of a company-bound field costs the statements of the same read of a
/// `compute_sudo = true` field on the same data (the hop's company is in the prefetched row).
#[tokio::test]
async fn the_company_bound_read_costs_what_the_sudo_read_costs() {
    let h = harness().await;
    let w = world(&h).await;
    let rows = [w.own, w.free];
    let mut counts = Vec::new();
    for field in ["leaf_name_sudo", "leaf_name"] {
        let mut tx = begin(&h, &[w.r], &[w.r]).await;
        let records = tx
            .env()
            .records(DOC)
            .unwrap()
            .browse(rows.iter().copied())
            .unwrap();
        let before = tx.query_count();
        let cells = records.cells(&mut tx, field).await.unwrap();
        counts.push(tx.query_count() - before);
        assert_eq!(cells.len(), rows.len());
    }
    assert!(counts[0] >= 1, "the sudo read runs statements");
    assert_eq!(
        counts[1], counts[0],
        "bound {} vs sudo {}",
        counts[1], counts[0]
    );
    h.cleanup().await.unwrap();
}

/// Catalogue and shared records in a root company and its branch, read by documents.
struct Catalogue {
    /// Documents reading a `parent_of` partner of the root / of the branch.
    partner_r: i64,
    partner_br: i64,
    /// Documents reading a `parent_of_any` shared record of the root / of the branch.
    shared_r: i64,
    shared_br: i64,
}

async fn catalogue(h: &Harness, w: &W) -> Catalogue {
    let mut tx = h.begin().await.unwrap();
    let partners = create(
        &mut tx,
        "test_company.partner",
        &[
            &[("name", text("PartnerR")), ("company_id", int(w.r))],
            &[("name", text("PartnerBr")), ("company_id", int(w.br))],
        ],
    )
    .await;
    let shared = create(
        &mut tx,
        "test_company.shared",
        &[
            &[("name", text("SharedR")), ("company_ids", ids(&[w.r]))],
            &[
                ("name", text("SharedBr")),
                ("company_ids", ids(&[w.br, w.o])),
            ],
        ],
    )
    .await;
    let docs = create(
        &mut tx,
        DOC,
        &[
            &[
                ("name", text("partner_r")),
                ("partner_id", int(partners.ids()[0])),
            ],
            &[
                ("name", text("partner_br")),
                ("partner_id", int(partners.ids()[1])),
            ],
            &[
                ("name", text("shared_r")),
                ("shared_id", int(shared.ids()[0])),
            ],
            &[
                ("name", text("shared_br")),
                ("shared_id", int(shared.ids()[1])),
            ],
        ],
    )
    .await;
    tx.commit().await.unwrap();
    Catalogue {
        partner_r: docs.ids()[0],
        partner_br: docs.ids()[1],
        shared_r: docs.ids()[2],
        shared_br: docs.ids()[3],
    }
}

/// A `parent_of` model accepts its company and the ancestors of an active company, not the
/// descendants; a `parent_of_any` model accepts a record when one of its companies does.
#[tokio::test]
async fn parent_forms_follow_the_company_tree() {
    let h = harness().await;
    let w = world(&h).await;
    let c = catalogue(&h, &w).await;
    let all = [w.r, w.br, w.o];

    // working in the branch: the root's records are usable, the branch's own are too
    let mut tx = begin(&h, &all, &[w.br]).await;
    for (doc, field, expected) in [
        (c.partner_r, "partner_name", "PartnerR"),
        (c.partner_br, "partner_name", "PartnerBr"),
        (c.shared_r, "shared_name", "SharedR"),
        (c.shared_br, "shared_name", "SharedBr"),
    ] {
        assert_eq!(
            string(&read(&mut tx, doc, field).await.unwrap()),
            expected,
            "{field}"
        );
    }
    drop(tx);

    // working in the root: the branch's records are not (a descendant is no ancestor)
    let mut tx = begin(&h, &all, &[w.r]).await;
    assert_eq!(
        string(&read(&mut tx, c.partner_r, "partner_name").await.unwrap()),
        "PartnerR"
    );
    assert_eq!(
        string(&read(&mut tx, c.shared_r, "shared_name").await.unwrap()),
        "SharedR"
    );
    let err = read(&mut tx, c.partner_br, "partner_name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert!(
        err.message().starts_with(&format!(
            "You cannot read “Partner Name” ({DOC}.partner_name) of Related Doc ({DOC}: {}): it \
             is read through Company Partner (test_company.partner: ",
            c.partner_br
        )),
        "{err}"
    );
    assert!(
        err.message().ends_with(
            "which belongs to a company outside your active companies.\n\nThis seems to be a \
             multi-company issue, you might be able to access the record by switching to the \
             company: Branch."
        ),
        "{err}"
    );
    // a shared record of several companies suggests the first of the user's in the order of
    // `res.company` (`sequence, name`, review RC9): by sequence, whatever the ids and names say
    for (other, branch, suggested) in [(1, 2, "Other"), (2, 1, "Branch")] {
        for (company, sequence) in [(w.o, other), (w.br, branch)] {
            tx.env()
                .records("res.company")
                .unwrap()
                .browse([company])
                .unwrap()
                .write_values(&mut tx, &map(&[("sequence", int(sequence))]))
                .await
                .unwrap();
        }
        let err = read(&mut tx, c.shared_br, "shared_name").await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");
        assert!(
            err.message().ends_with(&format!("company: {suggested}.")),
            "{err}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn search_ids(tx: &mut Transaction, sudo: bool, field: &str, value: &str) -> Vec<i64> {
    let mut records = tx.env().records(DOC).unwrap();
    if sudo {
        records = records.sudo(true);
    }
    let domain = erp_sdk::Domain::condition(field, erp_sdk::Operator::Eq, value).unwrap();
    assert_query_count!(tx, >= 1, <= 1, {
        records.search(tx, &domain).await.unwrap().ids().to_vec()
    })
}

/// A search on a company-bound related field cannot see through a foreign hop: the sub-domain of
/// each company-scoped hop is limited to the active companies. One statement, and the superuser
/// still finds everything.
#[tokio::test]
async fn search_on_the_related_field_excludes_foreign_hops() {
    let h = harness().await;
    let w = world(&h).await;
    let c = catalogue(&h, &w).await;
    let all = [w.r, w.br, w.o];
    let mut tx = begin(&h, &all, &[w.r]).await;
    // LeafR is read by `own` and, through the foreign mid, by `mid_foreign`
    assert_eq!(
        search_ids(&mut tx, true, "leaf_name", "LeafR").await,
        [w.own, w.mid_foreign]
    );
    assert_eq!(
        search_ids(&mut tx, false, "leaf_name", "LeafR").await,
        [w.own]
    );
    assert_eq!(
        search_ids(&mut tx, true, "leaf_name", "LeafO").await,
        [w.leaf_foreign]
    );
    assert!(
        search_ids(&mut tx, false, "leaf_name", "LeafO")
            .await
            .is_empty()
    );
    // the opt-out and the caller's own read are not bound
    assert_eq!(
        search_ids(&mut tx, false, "leaf_name_sudo", "LeafO").await,
        [w.leaf_foreign]
    );
    // a document whose chain ends without company still matches
    assert_eq!(
        search_ids(&mut tx, false, "leaf_name", "LeafNone").await,
        [w.free]
    );
    // the `parent_of` and `parent_of_any` forms
    assert!(
        search_ids(&mut tx, false, "partner_name", "PartnerBr")
            .await
            .is_empty()
    );
    assert_eq!(
        search_ids(&mut tx, true, "partner_name", "PartnerBr").await,
        [c.partner_br]
    );
    assert!(
        search_ids(&mut tx, false, "shared_name", "SharedBr")
            .await
            .is_empty()
    );
    assert_eq!(
        search_ids(&mut tx, true, "shared_name", "SharedBr").await,
        [c.shared_br]
    );
    drop(tx);

    // working in the branch, the root's catalogue and shared records match
    let mut tx = begin(&h, &all, &[w.br]).await;
    assert_eq!(
        search_ids(&mut tx, false, "partner_name", "PartnerR").await,
        [c.partner_r]
    );
    assert_eq!(
        search_ids(&mut tx, false, "shared_name", "SharedR").await,
        [c.shared_r]
    );
    assert_eq!(
        search_ids(&mut tx, false, "shared_name", "SharedBr").await,
        [c.shared_br]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Grouping by a company-bound related field puts the documents behind a foreign hop in the
/// empty group, as a hidden target of a many2one group-by.
#[tokio::test]
async fn group_by_the_related_field_excludes_foreign_hops() {
    let h = harness().await;
    let w = world(&h).await;
    let all = [w.r, w.br, w.o];
    let groups = |sudo: bool, active: Vec<i64>| {
        let (h, all) = (&h, all);
        async move {
            let mut tx = begin(h, &all, &active).await;
            let mut records = tx.env().records(DOC).unwrap();
            if sudo {
                records = records.sudo(true);
            }
            let rg = erp_sdk::ReadGroup::new(
                erp_sdk::Domain::condition(
                    "name",
                    erp_sdk::Operator::In,
                    vec!["own", "free", "mid_foreign", "leaf_foreign"],
                )
                .unwrap(),
            )
            .groupby(["leaf_name"])
            .aggregates(["__count"]);
            let groups = records._read_group(&mut tx, rg).await.unwrap();
            groups
                .rows()
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|v| v.to_value(erp_sdk::RecordsAs::Id))
                        .collect::<Vec<Value>>()
                })
                .collect::<Vec<_>>()
        }
    };
    // the superuser sees every leaf: LeafR twice (own, and through the foreign mid)
    assert_eq!(
        groups(true, vec![w.r]).await,
        [
            [text("LeafNone"), int(1)],
            [text("LeafO"), int(1)],
            [text("LeafR"), int(2)],
        ]
    );
    // the user in R: the two documents behind a foreign hop fall into the empty group
    assert_eq!(
        groups(false, vec![w.r]).await,
        [
            [text("LeafNone"), int(1)],
            [text("LeafR"), int(1)],
            [Value::False, int(2)],
        ]
    );
    // working in both R and O nothing is hidden
    assert_eq!(
        groups(false, vec![w.r, w.o]).await,
        groups(true, vec![w.r, w.o]).await
    );
    h.cleanup().await.unwrap();
}

/// An editable company-bound related field is written through its hop and reads back what was
/// written (the value cached for the reader's companies follows the write), and a write through a
/// foreign hop is not made easier by the bound: it is a write, decided by the caller's rights.
#[tokio::test]
async fn an_editable_bound_field_reads_back_its_write() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    assert_eq!(
        string(&read(&mut tx, w.own, "mid_name_edit").await.unwrap()),
        "MidR"
    );
    doc(&tx, w.own)
        .write_values(&mut tx, &map(&[("mid_name_edit", text("Renamed"))]))
        .await
        .unwrap();
    assert_eq!(
        string(&read(&mut tx, w.own, "mid_name_edit").await.unwrap()),
        "Renamed"
    );
    // the mid record itself carries the new name
    let mids = tx.env().records(MID).unwrap().browse([w.mid_r]).unwrap();
    assert_eq!(
        string(&mids.cells(&mut tx, "name").await.unwrap()[0]),
        "Renamed"
    );
    // a write through a foreign hop is decided by the caller's rights (the fixture policy allows
    // it), not by the bound: the foreign mid is renamed, and the field still cannot be read back
    // through that hop (review RC11)
    doc(&tx, w.mid_foreign)
        .write_values(&mut tx, &map(&[("mid_name_edit", text("Hijacked"))]))
        .await
        .unwrap();
    let foreign = tx.env().records(MID).unwrap().browse([w.mid_o]).unwrap();
    assert_eq!(
        string(&foreign.sudo(true).cells(&mut tx, "name").await.unwrap()[0]),
        "Hijacked"
    );
    tx.commit().await.unwrap();
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let err = read(&mut tx, w.mid_foreign, "mid_name_edit")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Mid Name Edit",
            "mid_name_edit",
            w.mid_foreign,
            "Related Mid",
            MID,
            Some(w.mid_o),
            OUTSIDE
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A stored related field is materialised when its record is written and read as a column: the
/// company bound applies to reading a path, and its sources are covered by the write-time check
/// (open question 4), so a value stored through a foreign hop reads back.
#[tokio::test]
async fn a_stored_related_field_is_not_bound_at_read_time() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    assert_eq!(
        string(
            &read(&mut tx, w.mid_foreign, "mid_name_stored")
                .await
                .unwrap()
        ),
        "MidO"
    );
    // computed while the user creates a document that points at the foreign mid
    let made = create(
        &mut tx,
        DOC,
        &[&[("name", text("made")), ("mid_id", int(w.mid_o))]],
    )
    .await;
    assert_eq!(
        string(
            &read(&mut tx, made.ids()[0], "mid_name_stored")
                .await
                .unwrap()
        ),
        "MidO"
    );
    let err = read(&mut tx, w.mid_foreign, "mid_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Mid Name",
            "mid_name",
            w.mid_foreign,
            "Related Mid",
            MID,
            Some(w.mid_o),
            OUTSIDE
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Records the scenarios below read through a holder (a model without company scope whose own
/// field is company-bound), in the companies of `world`.
struct Holders {
    /// Documents whose holder reads a leaf of the root / of the other company.
    own: i64,
    foreign: i64,
    holder_own: i64,
    holder_foreign: i64,
}

async fn holders(h: &Harness, w: &W) -> Holders {
    let mut tx = h.begin().await.unwrap();
    let mid_foreign_leaf = create(
        &mut tx,
        MID,
        &[&[
            ("name", text("MidRLeafO2")),
            ("company_id", int(w.r)),
            ("leaf_id", int(w.leaf_o)),
        ]],
    )
    .await
    .ids()[0];
    let held = create(
        &mut tx,
        "test_company_related.holder",
        &[
            &[("mid_id", int(w.mid_r))],
            &[("mid_id", int(mid_foreign_leaf))],
        ],
    )
    .await;
    let docs = create(
        &mut tx,
        DOC,
        &[
            &[
                ("name", text("holder_own")),
                ("holder_id", int(held.ids()[0])),
            ],
            &[
                ("name", text("holder_foreign")),
                ("holder_id", int(held.ids()[1])),
            ],
        ],
    )
    .await;
    tx.commit().await.unwrap();
    Holders {
        own: docs.ids()[0],
        foreign: docs.ids()[1],
        holder_own: held.ids()[0],
        holder_foreign: held.ids()[1],
    }
}

/// Review SC1: a related field whose path reads another company-bound related field follows that
/// field's path with the same bound. The holder has no company scope, so nothing on the document's
/// own path is checked: before the fix the inner field was read as superuser and the leaf of
/// another company came back (and was cached for every reader).
#[tokio::test]
async fn an_inner_company_bound_path_keeps_the_bound() {
    let h = harness().await;
    let w = world(&h).await;
    let k = holders(&h, &w).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    assert_eq!(
        string(&read(&mut tx, k.own, "holder_leaf_name").await.unwrap()),
        "LeafR"
    );
    let err = read(&mut tx, k.foreign, "holder_leaf_name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    // the field read is the document's; the hop is the inner path's second one (no id shown)
    let holder_message = hop_message(
        "Holder Leaf Name",
        "holder_leaf_name",
        k.foreign,
        "Related Leaf",
        LEAF,
        None,
        OUTSIDE,
    );
    assert_eq!(err.message(), holder_message);
    let su = doc(&tx, k.foreign).sudo(true);
    assert_eq!(
        string(&su.cells(&mut tx, "holder_leaf_name").await.unwrap()[0]),
        "LeafO"
    );
    let err = read(&mut tx, k.foreign, "holder_leaf_name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(err.message(), holder_message);
    // the search the reader can run sees only what a read can (the bound applies to the path)
    assert_eq!(
        search_ids(&mut tx, false, "holder_leaf_name", "LeafR").await,
        [k.own]
    );
    assert!(
        search_ids(&mut tx, false, "holder_leaf_name", "LeafO")
            .await
            .is_empty()
    );
    assert_eq!(
        search_ids(&mut tx, true, "holder_leaf_name", "LeafO").await,
        [k.foreign]
    );
    drop(tx);

    // in both companies the value is readable, and a search on it sees only what a read can
    let mut tx = begin(&h, &[w.r, w.o], &[w.r, w.o]).await;
    assert_eq!(
        string(&read(&mut tx, k.foreign, "holder_leaf_name").await.unwrap()),
        "LeafO"
    );
    assert_eq!(
        search_ids(&mut tx, false, "holder_leaf_name", "LeafO").await,
        [k.foreign]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SC1, the label path: the name of a link is the holder's record name, a company-bound
/// field.
#[tokio::test]
async fn the_label_of_a_link_keeps_the_bound_of_its_record_name() {
    let h = harness().await;
    let w = world(&h).await;
    let k = holders(&h, &w).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let rows = doc(&tx, k.own).read(&mut tx, &["holder_id"]).await.unwrap();
    assert_eq!(
        rows[0]["holder_id"],
        Value::list([Value::Int(k.holder_own), text("LeafR")])
    );
    let err = doc(&tx, k.foreign)
        .read(&mut tx, &["holder_id"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        format!(
            "You cannot read “Leaf Name” (test_company_related.holder.leaf_name) of Related \
             Holder (test_company_related.holder: {}): it is read through Related Leaf \
             ({LEAF}), which belongs to a company outside your active companies.\n\n{OUTSIDE}",
            k.holder_foreign
        )
    );
    let rows = doc(&tx, k.foreign)
        .sudo(true)
        .read(&mut tx, &["holder_id"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["holder_id"],
        Value::list([Value::Int(k.holder_foreign), text("LeafO")])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SC2: one foreign record among the prefetch siblings does not fail the read of the
/// records asked for. The records that pass are cached for the batch; a foreign record still
/// fails when it is the one asked for. (Before the fix the whole batch raised.)
#[tokio::test]
async fn a_foreign_prefetch_sibling_does_not_fail_the_read() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let all = tx
        .env()
        .records(DOC)
        .unwrap()
        .browse([w.own, w.free, w.mid_foreign, w.leaf_foreign])
        .unwrap();
    let rows: Vec<Records> = all.iter().collect();
    assert_eq!(
        string(&rows[0].cells(&mut tx, "leaf_name").await.unwrap()[0]),
        "LeafR"
    );
    // the sibling that passes was read in the same batch
    let free = assert_query_count!(tx, <= 0, {
        rows[1].cells(&mut tx, "leaf_name").await.unwrap()
    });
    assert_eq!(string(&free[0]), "LeafNone");
    // the foreign ones were not cached: asked for, they fail as they always did
    for (i, hop_label, hop_model, hop) in [
        (2, "Related Mid", MID, Some(w.mid_o)),
        (3, "Related Leaf", LEAF, None),
    ] {
        let err = rows[i].cells(&mut tx, "leaf_name").await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{err}");
        assert_eq!(
            err.message(),
            hop_message(
                "Leaf Name",
                "leaf_name",
                rows[i].ids()[0],
                hop_label,
                hop_model,
                hop,
                OUTSIDE
            )
        );
    }
    // and a foreign record asked for together with others still raises
    let err = all.cells(&mut tx, "mid_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Mid Name",
            "mid_name",
            w.mid_foreign,
            "Related Mid",
            MID,
            Some(w.mid_o),
            OUTSIDE
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review RC3: the hops' company fields are dependencies of a company-bound value. A hop that
/// moves to another company drops the cached value (it was readable, it no longer is); moving it
/// back lets the value be read again.
#[tokio::test]
async fn a_hop_moving_to_another_company_drops_the_cached_value() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r, w.o], &[w.r]).await;
    assert_eq!(
        string(&read(&mut tx, w.own, "mid_name").await.unwrap()),
        "MidR"
    );
    let mid = tx.env().records(MID).unwrap().browse([w.mid_r]).unwrap();
    mid.write_values(&mut tx, &map(&[("company_id", int(w.o))]))
        .await
        .unwrap();
    let err = read(&mut tx, w.own, "mid_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Mid Name",
            "mid_name",
            w.own,
            "Related Mid",
            MID,
            Some(w.mid_r),
            "This seems to be a multi-company issue, you might be able to access the record by \
             switching to the company: Other."
        )
    );
    mid.write_values(&mut tx, &map(&[("company_id", int(w.r))]))
        .await
        .unwrap();
    assert_eq!(
        string(&read(&mut tx, w.own, "mid_name").await.unwrap()),
        "MidR"
    );
    // the second hop's company counts as well
    assert_eq!(
        string(&read(&mut tx, w.own, "leaf_name").await.unwrap()),
        "LeafR"
    );
    let leaf = tx.env().records(LEAF).unwrap().browse([w.leaf_r]).unwrap();
    leaf.write_values(&mut tx, &map(&[("company_id", int(w.o))]))
        .await
        .unwrap();
    let err = read(&mut tx, w.own, "leaf_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Leaf Name",
            "leaf_name",
            w.own,
            "Related Leaf",
            LEAF,
            None,
            SWITCH_TO_OTHER
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review RC5: the parent forms cost the statements of the same read with `compute_sudo = true`
/// plus the reads the match needs: the active companies' `parent_path` (one statement), and for a
/// many2many company field the link rows of the hops (one more). The hop's own company column is
/// in the prefetched row.
#[tokio::test]
async fn the_parent_forms_cost_the_sudo_read_plus_the_companies_tree() {
    let h = harness().await;
    let w = world(&h).await;
    let c = catalogue(&h, &w).await;
    for (rows, bound, sudo, extra) in [
        (
            [c.partner_r, c.partner_br],
            "partner_name",
            "partner_name_sudo",
            1,
        ),
        (
            [c.shared_r, c.shared_br],
            "shared_name",
            "shared_name_sudo",
            2,
        ),
    ] {
        let mut counts = Vec::new();
        for field in [sudo, bound] {
            let mut tx = begin(&h, &[w.r, w.br, w.o], &[w.br, w.o]).await;
            let records = tx
                .env()
                .records(DOC)
                .unwrap()
                .browse(rows.iter().copied())
                .unwrap();
            let before = tx.query_count();
            let cells = records.cells(&mut tx, field).await.unwrap();
            counts.push(tx.query_count() - before);
            assert_eq!(cells.len(), rows.len());
            if field == bound {
                // warm: the tree is cached for the transaction
                let again = assert_query_count!(tx, <= 0, {
                    records.cells(&mut tx, field).await.unwrap()
                });
                assert_eq!(again, cells);
            }
        }
        assert!(counts[0] >= 1, "{sudo} runs statements");
        assert_eq!(
            counts[1],
            counts[0] + extra,
            "{bound}: {} statements, {sudo}: {}",
            counts[1],
            counts[0]
        );
    }
    h.cleanup().await.unwrap();
}

async fn group_rows(tx: &mut Transaction, field: &str) -> Vec<Vec<Value>> {
    let records = tx.env().records(DOC).unwrap();
    let rg = erp_sdk::ReadGroup::new(
        erp_sdk::Domain::condition(
            "name",
            erp_sdk::Operator::In,
            vec!["own", "free", "mid_foreign", "leaf_foreign"],
        )
        .unwrap(),
    )
    .groupby([field])
    .aggregates(["__count"]);
    let groups = records._read_group(tx, rg).await.unwrap();
    groups
        .rows()
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| v.to_value(erp_sdk::RecordsAs::Id))
                .collect::<Vec<Value>>()
        })
        .collect()
}

/// Review SC3: the join of a bound group-by reads the hop's company column, so a pending write
/// to it is flushed first; the statement sees the document's mid in its new company.
#[tokio::test]
async fn group_by_flushes_the_company_of_the_hops() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r, w.o], &[w.r]).await;
    let before = group_rows(&mut tx, "mid_name").await;
    assert!(before.contains(&vec![text("MidR"), int(1)]), "{before:?}");
    // pending: not yet in the database when the group-by is built
    tx.env()
        .records(MID)
        .unwrap()
        .browse([w.mid_r])
        .unwrap()
        .write_values(&mut tx, &map(&[("company_id", int(w.o))]))
        .await
        .unwrap();
    let after = group_rows(&mut tx, "mid_name").await;
    assert!(!after.contains(&vec![text("MidR"), int(1)]), "{after:?}");
    assert!(after.contains(&vec![Value::False, int(2)]), "{after:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SC4 for a group-by: the record a bound many2one path ends on is one more hop. The
/// leaf of another company falls into the empty group (before the fix the group key was its id).
#[tokio::test]
async fn group_by_a_bound_many2one_hides_a_foreign_last_target() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let rows = group_rows(&mut tx, "leaf_id").await;
    assert!(rows.contains(&vec![int(w.leaf_r), int(1)]), "{rows:?}");
    assert!(
        !rows.iter().any(|r| r[0] == int(w.leaf_o)),
        "the foreign leaf is a group: {rows:?}"
    );
    // the documents behind a foreign mid or ending on a foreign leaf
    assert!(rows.contains(&vec![Value::False, int(2)]), "{rows:?}");
    drop(tx);

    let mut tx = begin(&h, &[w.r, w.o], &[w.r, w.o]).await;
    let rows = group_rows(&mut tx, "leaf_id").await;
    assert!(rows.contains(&vec![int(w.leaf_o), int(1)]), "{rows:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SC4 for a search: a value of a bound many2one path is only found when the record it
/// points to is one the reader may pass through.
#[tokio::test]
async fn search_on_a_bound_many2one_ignores_a_foreign_last_target() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r, w.o], &[w.r]).await;
    let by = |value: i64| erp_sdk::Domain::condition("leaf_id", erp_sdk::Operator::Eq, value);
    let records = tx.env().records(DOC).unwrap();
    let found = records
        .search(&mut tx, &by(w.leaf_o).unwrap())
        .await
        .unwrap();
    assert!(found.ids().is_empty(), "{:?}", found.ids());
    let found = records
        .sudo(true)
        .search(&mut tx, &by(w.leaf_o).unwrap())
        .await
        .unwrap();
    assert_eq!(found.ids(), [w.leaf_foreign]);
    let found = records
        .search(&mut tx, &by(w.leaf_r).unwrap())
        .await
        .unwrap();
    assert_eq!(found.ids(), [w.own]);
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn find_tags(
    tx: &mut Transaction,
    sudo: bool,
    operator: erp_sdk::Operator,
    value: impl Into<erp_sdk::domain::Value>,
) -> Vec<i64> {
    let mut records = tx.env().records(DOC).unwrap();
    if sudo {
        records = records.sudo(true);
    }
    let domain = erp_sdk::Domain::condition("tag_rel_ids", operator, value).unwrap();
    records.search(tx, &domain).await.unwrap().ids().to_vec()
}

fn kid(tx: &Transaction, id: i64) -> Records {
    tx.env().records(KID).unwrap().browse([id]).unwrap()
}

async fn kid_cell(tx: &mut Transaction, id: i64, field: &str) -> erp_sdk::Result<Cell> {
    let cells = kid(tx, id).cells(tx, field).await?;
    Ok(cells.into_iter().next().unwrap())
}

/// Review RD1: a company-bound path whose first step is a field of an `_inherits` parent. The
/// trigger paths of the hops' company fields are built from the steps with the delegate expanded
/// (before the fix they started with a field the child only inherits, and the registry refused
/// the model: "neither stored nor related"). The bound applies on read, and a hop that moves to
/// another company drops the cached value.
#[tokio::test]
async fn a_bound_path_through_an_inherited_field_reads_and_invalidates() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = h.begin().await.unwrap();
    let kids = create(
        &mut tx,
        KID,
        &[
            &[("doc_id", int(w.own))],
            &[("doc_id", int(w.mid_foreign))],
            &[("doc_id", int(w.leaf_foreign))],
        ],
    )
    .await;
    tx.commit().await.unwrap();
    let (own, mid_foreign, leaf_foreign) = (kids.ids()[0], kids.ids()[1], kids.ids()[2]);
    let message =
        |field: &str, label: &str, id: i64, hop: (&str, &str, Option<i64>), hint: &str| {
            hop_message_on((KID, "Related Kid"), label, field, id, hop, hint)
        };

    let mut tx = begin(&h, &[w.r, w.o], &[w.r]).await;
    let cell = kid_cell;
    assert_eq!(
        string(&cell(&mut tx, own, "kid_mid_name").await.unwrap()),
        "MidR"
    );
    assert_eq!(
        string(&cell(&mut tx, own, "kid_leaf_name").await.unwrap()),
        "LeafR"
    );
    let err = cell(&mut tx, mid_foreign, "kid_mid_name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        message(
            "kid_mid_name",
            "Kid Mid Name",
            mid_foreign,
            ("Related Mid", MID, Some(w.mid_o)),
            SWITCH_TO_OTHER
        )
    );
    let err = cell(&mut tx, leaf_foreign, "kid_leaf_name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        message(
            "kid_leaf_name",
            "Kid Leaf Name",
            leaf_foreign,
            ("Related Leaf", LEAF, None),
            SWITCH_TO_OTHER
        )
    );

    // the cached value does not survive its hop moving to another company, and comes back
    let mid = tx.env().records(MID).unwrap().browse([w.mid_r]).unwrap();
    mid.write_values(&mut tx, &map(&[("company_id", int(w.o))]))
        .await
        .unwrap();
    let err = cell(&mut tx, own, "kid_mid_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        message(
            "kid_mid_name",
            "Kid Mid Name",
            own,
            ("Related Mid", MID, Some(w.mid_r)),
            SWITCH_TO_OTHER
        )
    );
    mid.write_values(&mut tx, &map(&[("company_id", int(w.r))]))
        .await
        .unwrap();
    assert_eq!(
        string(&cell(&mut tx, own, "kid_mid_name").await.unwrap()),
        "MidR"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review RD3: a one-segment alias of a link to a company-scoped model is bound like the
/// longer paths: the read refuses the foreign target (before the fix it returned its id, while
/// the search and the group-by already hid it).
#[tokio::test]
async fn a_one_segment_alias_reads_searches_and_groups_alike() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    assert_eq!(
        read(&mut tx, w.own, "mid_alias_id").await.unwrap(),
        read(&mut tx, w.own, "mid_id").await.unwrap()
    );
    let err = read(&mut tx, w.mid_foreign, "mid_alias_id")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Mid Alias",
            "mid_alias_id",
            w.mid_foreign,
            "Related Mid",
            MID,
            Some(w.mid_o),
            OUTSIDE
        )
    );
    // the search finds nothing behind the foreign target, the superuser still does
    let by = erp_sdk::Domain::condition("mid_alias_id", erp_sdk::Operator::Eq, w.mid_o).unwrap();
    let records = tx.env().records(DOC).unwrap();
    assert!(records.search(&mut tx, &by).await.unwrap().ids().is_empty());
    assert_eq!(
        records.sudo(true).search(&mut tx, &by).await.unwrap().ids(),
        [w.mid_foreign]
    );
    // and no group is keyed by the foreign target
    let rows = group_rows(&mut tx, "mid_alias_id").await;
    assert!(rows.contains(&vec![int(w.mid_r), int(1)]), "{rows:?}");
    assert!(!rows.iter().any(|r| r[0] == int(w.mid_o)), "{rows:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SD1: a search on a bound path that ends on several company-scoped records agrees with
/// the read, which refuses a record with any foreign target: a record with one foreign and one
/// own target is not found by either of them (before the fix it was found by the foreign one,
/// so the search probed ids and names the read refuses).
#[tokio::test]
async fn search_on_a_bound_x2many_excludes_records_with_a_foreign_target() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = h.begin().await.unwrap();
    let tags = create(
        &mut tx,
        TAG,
        &[
            &[("name", text("TagForeign")), ("company_id", int(w.o))],
            &[("name", text("TagOwn")), ("company_id", int(w.r))],
        ],
    )
    .await;
    let (foreign, own) = (tags.ids()[0], tags.ids()[1]);
    let mids = create(
        &mut tx,
        MID,
        &[
            &[
                ("name", text("MixedTags")),
                ("tag_ids", ids(&[foreign, own])),
            ],
            &[("name", text("OwnTags")), ("tag_ids", ids(&[own]))],
        ],
    )
    .await;
    let docs = create(
        &mut tx,
        DOC,
        &[
            &[("name", text("mixed")), ("mid_id", int(mids.ids()[0]))],
            &[("name", text("own_tags")), ("mid_id", int(mids.ids()[1]))],
        ],
    )
    .await;
    tx.commit().await.unwrap();
    let (mixed, own_tags) = (docs.ids()[0], docs.ids()[1]);

    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    // the foreign tag is not found, by id or by name
    assert!(
        find_tags(&mut tx, false, erp_sdk::Operator::In, vec![foreign])
            .await
            .is_empty()
    );
    assert!(
        find_tags(&mut tx, false, erp_sdk::Operator::ILike, "Foreign")
            .await
            .is_empty()
    );
    // the own tag finds only the record whose targets are all usable
    assert_eq!(
        find_tags(&mut tx, false, erp_sdk::Operator::In, vec![own]).await,
        [own_tags]
    );
    assert_eq!(
        find_tags(&mut tx, false, erp_sdk::Operator::ILike, "TagOwn").await,
        [own_tags]
    );
    // the superuser sees everything
    assert_eq!(
        find_tags(&mut tx, true, erp_sdk::Operator::In, vec![foreign]).await,
        [mixed]
    );
    // and the read of the record with a foreign target is refused
    let err = read(&mut tx, mixed, "tag_rel_ids").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message(
            "Tag Rel",
            "tag_rel_ids",
            mixed,
            "Company Tag",
            TAG,
            None,
            OUTSIDE
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A world for the empty-value searches (review FX1, FX2): a doc without a mid, a doc whose mid has
/// no tags, a doc whose mid has only a foreign tag, a doc whose mid has an own tag.
struct EmptyWorld {
    no_mid: i64,
    no_tags: i64,
    foreign_only: i64,
    own_tag: i64,
    foreign: i64,
    own: i64,
}

async fn empty_world(h: &Harness, w: &W) -> EmptyWorld {
    let mut tx = h.begin().await.unwrap();
    let tags = create(
        &mut tx,
        TAG,
        &[
            &[("name", text("TagForeign")), ("company_id", int(w.o))],
            &[("name", text("TagOwn")), ("company_id", int(w.r))],
        ],
    )
    .await;
    let (foreign, own) = (tags.ids()[0], tags.ids()[1]);
    let mids = create(
        &mut tx,
        MID,
        &[
            &[("name", text("NoTags"))],
            &[("name", text("ForeignOnly")), ("tag_ids", ids(&[foreign]))],
            &[("name", text("OwnTag")), ("tag_ids", ids(&[own]))],
        ],
    )
    .await;
    let docs = create(
        &mut tx,
        DOC,
        &[
            &[("name", text("no_mid"))],
            &[("name", text("no_tags")), ("mid_id", int(mids.ids()[0]))],
            &[
                ("name", text("foreign_only")),
                ("mid_id", int(mids.ids()[1])),
            ],
            &[("name", text("own_tag")), ("mid_id", int(mids.ids()[2]))],
        ],
    )
    .await;
    tx.commit().await.unwrap();
    EmptyWorld {
        no_mid: docs.ids()[0],
        no_tags: docs.ids()[1],
        foreign_only: docs.ids()[2],
        own_tag: docs.ids()[3],
        foreign,
        own,
    }
}

/// The documents of `ids` among the ones `find` returns, sorted.
fn only(found: Vec<i64>, of: &[i64]) -> Vec<i64> {
    let mut found: Vec<i64> = found.into_iter().filter(|i| of.contains(i)).collect();
    found.sort_unstable();
    found
}

/// Review FX1: a sub-domain value is not an empty value. Before the fix `tag_rel_ids any []`
/// became `mid_id any (tag_ids any []) | mid_id = False` (a record without mid "has tags"),
/// `not any []` left out the records without mid although they have no tags, and the `any []`
/// form skipped the bound of the last field (a mid with only a foreign tag was found).
#[tokio::test]
async fn any_with_an_empty_domain_on_a_bound_x2many_is_not_an_empty_value() {
    let h = harness().await;
    let w = world(&h).await;
    let e = empty_world(&h, &w).await;
    let all = [e.no_mid, e.no_tags, e.foreign_only, e.own_tag];
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let any = erp_sdk::Operator::Any;
    let not_any = erp_sdk::Operator::NotAny;
    let empty = || DV::Domain(erp_sdk::Domain::TRUE);
    // has a tag the reader may use: only the record with an own tag (the record with only a
    // foreign tag is refused by the read, so it is not found either)
    let found = find_tags(&mut tx, false, any, empty()).await;
    assert_eq!(only(found, &all), [e.own_tag]);
    // has no tag, or one the reader may not see: the other three
    let found = find_tags(&mut tx, false, not_any, empty()).await;
    let mut expected = vec![e.no_mid, e.no_tags, e.foreign_only];
    expected.sort_unstable();
    assert_eq!(only(found, &all), expected);
    // the superuser has no bound
    let found = find_tags(&mut tx, true, any, empty()).await;
    let mut expected = vec![e.foreign_only, e.own_tag];
    expected.sort_unstable();
    assert_eq!(only(found, &all), expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review FX2: `in [False, F]` holds for an empty value or for F, and F keeps the bound. Before
/// the fix a foreign F was found through an own hop (the bound was skipped), while `in [F]`
/// found nothing and the read refuses.
#[tokio::test]
async fn in_with_false_and_a_foreign_target_keeps_the_bound() {
    let h = harness().await;
    let w = world(&h).await;
    let e = empty_world(&h, &w).await;
    let all = [e.no_mid, e.no_tags, e.foreign_only, e.own_tag];
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let set = |rest: i64| DV::list([DV::FALSE, DV::Int(rest)]);
    // many2many: empty tags (no mid, a mid without tags) or the tag; the foreign tag adds nothing
    let found = find_tags(&mut tx, false, erp_sdk::Operator::In, set(e.foreign)).await;
    let mut expected = vec![e.no_mid, e.no_tags];
    expected.sort_unstable();
    assert_eq!(only(found, &all), expected);
    let found = find_tags(&mut tx, false, erp_sdk::Operator::In, set(e.own)).await;
    let mut expected = vec![e.no_mid, e.no_tags, e.own_tag];
    expected.sort_unstable();
    assert_eq!(only(found, &all), expected);
    // many2one: leaf_id of a doc is the mid's leaf; leaf_foreign's mid points to a leaf in O
    let by =
        |value: DV| erp_sdk::Domain::condition("leaf_id", erp_sdk::Operator::In, value).unwrap();
    let records = tx.env().records(DOC).unwrap();
    let found = records.search(&mut tx, &by(set(w.leaf_o))).await.unwrap();
    assert!(
        !found.ids().contains(&w.leaf_foreign),
        "found {:?}",
        found.ids()
    );
    let found = records.search(&mut tx, &by(set(w.leaf_r))).await.unwrap();
    assert!(found.ids().contains(&w.own), "{:?}", found.ids());
    assert!(!found.ids().contains(&w.leaf_foreign));
    // the superuser has no bound
    let found = records
        .sudo(true)
        .search(&mut tx, &by(set(w.leaf_o)))
        .await
        .unwrap();
    assert!(found.ids().contains(&w.leaf_foreign));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review RD4: the last field of a bound path that points to a company-scoped model costs one
/// more statement than the superuser read on a cold cache: the companies of the records it
/// points to (the first read of them in the transaction). Warm: none.
#[tokio::test]
async fn a_relational_last_field_costs_one_company_read_more_than_the_sudo_read() {
    let h = harness().await;
    let w = world(&h).await;
    let rows = [w.own, w.free];
    let mut counts = Vec::new();
    for field in ["leaf_id_sudo", "leaf_id"] {
        let mut tx = begin(&h, &[w.r], &[w.r]).await;
        let records = tx
            .env()
            .records(DOC)
            .unwrap()
            .browse(rows.iter().copied())
            .unwrap();
        let before = tx.query_count();
        let cells = records.cells(&mut tx, field).await.unwrap();
        counts.push(tx.query_count() - before);
        assert_eq!(cells.len(), rows.len());
        let again = assert_query_count!(tx, <= 0, {
            records.cells(&mut tx, field).await.unwrap()
        });
        assert_eq!(again, cells);
    }
    assert!(counts[0] >= 1, "the sudo read runs statements");
    assert_eq!(
        counts[1],
        counts[0] + 1,
        "bound {} vs sudo {}",
        counts[1],
        counts[0]
    );
    h.cleanup().await.unwrap();
}

/// Records read through links: the labels of links to a holder (a record whose record name is
/// company-bound) and a link to that holder (whose record name is the holder's label).
struct Links {
    /// Documents whose link / holder reads a record of the root / of the other company.
    own: i64,
    foreign: i64,
    link_own: i64,
    link_foreign: i64,
    holder_foreign: i64,
    holder1_own: i64,
    holder1_foreign: i64,
}

async fn links(h: &Harness, w: &W, k: &Holders) -> Links {
    let mut tx = h.begin().await.unwrap();
    let link_rows = create(
        &mut tx,
        "test_company_related.link",
        &[
            &[("holder_id", int(k.holder_own))],
            &[("holder_id", int(k.holder_foreign))],
        ],
    )
    .await;
    let holders = create(
        &mut tx,
        HOLDER1,
        &[&[("mid_id", int(w.mid_r))], &[("mid_id", int(w.mid_o))]],
    )
    .await;
    let docs = create(
        &mut tx,
        DOC,
        &[
            &[
                ("name", text("link_own")),
                ("link_id", int(link_rows.ids()[0])),
                ("holder1_id", int(holders.ids()[0])),
            ],
            &[
                ("name", text("link_foreign")),
                ("link_id", int(link_rows.ids()[1])),
                ("holder1_id", int(holders.ids()[1])),
            ],
        ],
    )
    .await;
    tx.commit().await.unwrap();
    Links {
        own: docs.ids()[0],
        foreign: docs.ids()[1],
        link_own: link_rows.ids()[0],
        link_foreign: link_rows.ids()[1],
        holder_foreign: k.holder_foreign,
        holder1_own: holders.ids()[0],
        holder1_foreign: holders.ids()[1],
    }
}

/// Review SD3: the label of a link that is itself the record name of another record goes through
/// two levels (link, holder). The reader's bound reaches the inner level: before the fix the
/// holder's record name was read as superuser one level down and the foreign leaf's name came
/// back as the label.
#[tokio::test]
async fn a_label_two_levels_down_keeps_the_bound() {
    let h = harness().await;
    let w = world(&h).await;
    let k = holders(&h, &w).await;
    let l = links(&h, &w, &k).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let rows = doc(&tx, l.own).read(&mut tx, &["link_id"]).await.unwrap();
    assert_eq!(
        rows[0]["link_id"],
        Value::list([Value::Int(l.link_own), text("LeafR")])
    );
    let err = doc(&tx, l.foreign)
        .read(&mut tx, &["link_id"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(
        err.message(),
        hop_message_on(
            (HOLDER, "Related Holder"),
            "Leaf Name",
            "leaf_name",
            l.holder_foreign,
            ("Related Leaf", LEAF, None),
            OUTSIDE
        )
    );
    // the link's own display name takes the same way
    let link = tx
        .env()
        .records("test_company_related.link")
        .unwrap()
        .browse([l.link_foreign])
        .unwrap();
    let err = link.cells(&mut tx, "display_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    let rows = doc(&tx, l.foreign)
        .sudo(true)
        .read(&mut tx, &["link_id"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["link_id"],
        Value::list([Value::Int(l.link_foreign), text("LeafO")])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SD4: a label whose record name is bound through one hop fails with an error that does
/// not name the hop's id (the reader reached the holder through a link and may not read the hop
/// record), and one foreign holder among the links read refuses the whole read, loudly (no
/// placeholder label).
#[tokio::test]
async fn a_label_refusal_names_no_hop_and_refuses_the_whole_read() {
    let h = harness().await;
    let w = world(&h).await;
    let k = holders(&h, &w).await;
    let l = links(&h, &w, &k).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let rows = doc(&tx, l.own)
        .read(&mut tx, &["holder1_id"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["holder1_id"],
        Value::list([Value::Int(l.holder1_own), text("MidR")])
    );
    let expected = hop_message_on(
        (HOLDER1, "Related Holder One"),
        "Mid Name",
        "mid_name",
        l.holder1_foreign,
        ("Related Mid", MID, None),
        OUTSIDE,
    );
    let err = doc(&tx, l.foreign)
        .read(&mut tx, &["holder1_id"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(err.message(), expected);
    // the passing record read together with the foreign one: the whole read fails the same way
    let both = tx
        .env()
        .records(DOC)
        .unwrap()
        .browse([l.own, l.foreign])
        .unwrap();
    let err = both.read(&mut tx, &["holder1_id"]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    assert_eq!(err.message(), expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review TD3: whether a refusal names the hop is decided by how the record was reached, carried
/// by the environment of the read (no counter in the engine). The label of a link never names the
/// hop id, and a direct read of the same field right after, in the same transaction, still does;
/// then the label again, after the direct read.
#[tokio::test]
async fn a_direct_read_names_the_hop_after_a_label_read_did_not() {
    let h = harness().await;
    let w = world(&h).await;
    let k = holders(&h, &w).await;
    let l = links(&h, &w, &k).await;
    let mut tx = begin(&h, &[w.r], &[w.r]).await;
    let by_label = hop_message_on(
        (HOLDER1, "Related Holder One"),
        "Mid Name",
        "mid_name",
        l.holder1_foreign,
        ("Related Mid", MID, None),
        OUTSIDE,
    );
    let by_id = hop_message_on(
        (HOLDER1, "Related Holder One"),
        "Mid Name",
        "mid_name",
        l.holder1_foreign,
        ("Related Mid", MID, Some(w.mid_o)),
        OUTSIDE,
    );
    let holder = tx
        .env()
        .records(HOLDER1)
        .unwrap()
        .browse([l.holder1_foreign])
        .unwrap();
    for (what, expected) in [
        ("label", &by_label),
        ("direct", &by_id),
        ("label again", &by_label),
    ] {
        let err = if what == "direct" {
            holder.cells(&mut tx, "mid_name").await.unwrap_err()
        } else {
            doc(&tx, l.foreign)
                .read(&mut tx, &["holder1_id"])
                .await
                .unwrap_err()
        };
        assert_eq!(err.kind(), ErrorKind::Access, "{what}: {err}");
        assert_eq!(err.message(), *expected, "{what}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The group-by of `partner_name` over the documents `ids` (a `parent_of` company-bound related
/// path), as `_read_group`.
fn partner_name_groups(ids: Vec<i64>) -> erp_sdk::ReadGroup {
    erp_sdk::ReadGroup::new(erp_sdk::Domain::condition("id", erp_sdk::Operator::In, ids).unwrap())
        .groupby(["partner_name"])
        .aggregates(["__count"])
}

/// Review SF4 and FV1: an active company whose `parent_path` is empty or NULL (a damaged tree) is
/// refused by the group-by with the very error the read raises (`CompanyError::MalformedParentPath`,
/// kind and message), not silently left without ancestors and not as a database cast failure.
#[tokio::test]
async fn group_by_refuses_an_active_company_without_a_parent_path() {
    let h = harness().await;
    let w = world(&h).await;
    let c = catalogue(&h, &w).await;
    let all = [w.r, w.br, w.o];
    for damaged in ["NULL", "''"] {
        let mut tx = h.begin().await.unwrap();
        tx.batch(&format!(
            "UPDATE res_company SET parent_path = {damaged} WHERE id = {}",
            w.br
        ))
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let mut tx = begin(&h, &all, &[w.br]).await;
        let expected = format!(
            "res.company,{} has an empty or malformed parent_path; the company hierarchy cannot \
             be checked",
            w.br
        );
        let read_err = read(&mut tx, c.partner_r, "partner_name")
            .await
            .unwrap_err();
        assert_eq!(
            read_err.kind(),
            ErrorKind::Database,
            "{damaged}: {read_err}"
        );
        assert!(
            read_err.message().contains(&expected),
            "{damaged}: {read_err}"
        );
        let records = tx.env().records(DOC).unwrap();
        let rg = partner_name_groups(vec![c.partner_r, c.partner_br]);
        let err = records._read_group(&mut tx, rg).await.unwrap_err();
        assert_eq!(err.kind(), read_err.kind(), "{damaged}: {err}");
        assert_eq!(err.message(), read_err.message(), "{damaged}");
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// Review FV1: the refusal does not depend on the data. No document reaches a hop with a
/// company (the partners have none), yet the active company's path is damaged: the read and the
/// group-by raise the same error.
#[tokio::test]
async fn group_by_refuses_a_damaged_path_when_no_hop_has_a_company() {
    let h = harness().await;
    let w = world(&h).await;
    let mut tx = h.begin().await.unwrap();
    let partners = create(
        &mut tx,
        "test_company.partner",
        &[&[("name", text("NoCompany"))]],
    )
    .await;
    let docs = create(
        &mut tx,
        DOC,
        &[&[
            ("name", text("no_company")),
            ("partner_id", int(partners.ids()[0])),
        ]],
    )
    .await;
    let doc_id = docs.ids()[0];
    tx.batch(&format!(
        "UPDATE res_company SET parent_path = NULL WHERE id = {}",
        w.br
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = begin(&h, &[w.r, w.br, w.o], &[w.br]).await;
    let read_err = read(&mut tx, doc_id, "partner_name").await.unwrap_err();
    let records = tx.env().records(DOC).unwrap();
    let err = records
        ._read_group(&mut tx, partner_name_groups(vec![doc_id]))
        .await
        .unwrap_err();
    assert_eq!(read_err.kind(), ErrorKind::Database, "{read_err}");
    assert!(
        read_err.message().contains(&format!(
            "res.company,{} has an empty or malformed parent_path",
            w.br
        )),
        "{read_err}"
    );
    assert_eq!(err.kind(), read_err.kind(), "{err}");
    assert_eq!(err.message(), read_err.message());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review FV1: the ancestor set of a parent-matched group-by costs at most one statement the first
/// time (the active companies' parent paths) and none after (cached in the transaction).
#[tokio::test]
async fn group_by_parent_closure_costs_one_statement_cold_none_warm() {
    let h = harness().await;
    let w = world(&h).await;
    let c = catalogue(&h, &w).await;
    let mut tx = begin(&h, &[w.r, w.br, w.o], &[w.br]).await;
    let records = tx.env().records(DOC).unwrap();
    let ids = vec![c.partner_r, c.partner_br];
    let plain = || {
        erp_sdk::ReadGroup::new(
            erp_sdk::Domain::condition("id", erp_sdk::Operator::In, vec![c.partner_r]).unwrap(),
        )
        .groupby(["name"])
        .aggregates(["__count"])
    };
    records._read_group(&mut tx, plain()).await.unwrap();
    let before = tx.query_count();
    records._read_group(&mut tx, plain()).await.unwrap();
    let baseline = tx.query_count() - before;
    let before = tx.query_count();
    records
        ._read_group(&mut tx, partner_name_groups(ids.clone()))
        .await
        .unwrap();
    let cold = tx.query_count() - before;
    let before = tx.query_count();
    records
        ._read_group(&mut tx, partner_name_groups(ids))
        .await
        .unwrap();
    let warm = tx.query_count() - before;
    assert!(cold <= baseline + 1, "cold {cold} vs plain {baseline}");
    assert_eq!(warm, baseline, "warm {warm} vs plain {baseline}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A change of a user's companies marks the transaction for re-resolution (what the base addon
/// registers for `res.users`).
static USERS: erp_orm::PolicySourceWrite =
    erp_orm::PolicySourceWrite::new("res.users", erp_orm::SourceKind::Membership)
        .fields(&["company_ids"]);

/// A harness with the real principal resolver over `res.users` and `res.company`, so the
/// subject's companies are validated against the database (review SF1).
async fn resolver_harness() -> Harness {
    let addons = || {
        [
            &base_users::ADDON,
            &test_company::ADDON,
            &test_company_related::ADDON,
        ]
    };
    let registry = Harness::builder(addons()).registry_only().unwrap();
    let resolver = erp_security::resolver::Resolver::for_registry(&registry).unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow).build().unwrap();
    Harness::builder(addons())
        .policy(policy)
        .resolver(resolver)
        .policy_sources([&USERS])
        .build()
        .await
        .expect("harness")
}

/// Make the user of the scenarios (`DEMO`) a member of `member_of`, the first being the main
/// company.
async fn enrol(h: &Harness, member_of: &[i64]) {
    let mut tx = h.begin().await.unwrap();
    // the resolver reads a main company and the group links, which these fixtures do not declare
    tx.batch(
        "ALTER TABLE res_users ADD COLUMN company_id int8; \
         CREATE TABLE res_groups_users_rel (uid int8 NOT NULL, gid int8 NOT NULL)",
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE res_users SET company_id = $1 WHERE id = $2",
        &[&member_of[0], &DEMO],
    )
    .await
    .unwrap();
    for cid in member_of {
        tx.execute(
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
            &[&DEMO, cid],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

async fn docs_where(
    tx: &mut Transaction,
    env: &erp_sdk::Env,
    field: &str,
    operator: erp_sdk::Operator,
    value: impl Into<erp_sdk::domain::Value>,
) -> erp_sdk::Result<Vec<i64>> {
    let domain = erp_sdk::Domain::condition(field, operator, value).unwrap();
    let found = env.records(DOC)?.search(tx, &domain).await?;
    Ok(found.ids().to_vec())
}

/// Review SF1 (a): a company the user lost earlier in the transaction is refused by a search that
/// still names it, as it is by a read. Pins the behaviour: it already held before the fix (the
/// record rules' binding refused the company first), so it did not fail on the old code.
#[tokio::test]
async fn search_refuses_a_company_removed_earlier_in_the_transaction() {
    let h = resolver_harness().await;
    let w = world(&h).await;
    enrol(&h, &[w.r, w.o]).await;
    let mut tx = h.begin_as(DEMO).await.unwrap();
    let env = tx
        .env()
        .with_context([("allowed_company_ids", ids(&[w.r, w.o]))]);
    for (field, operator, value) in [
        ("mid_name", erp_sdk::Operator::Eq, "MidO"),
        ("tag_rel_ids", erp_sdk::Operator::ILike, "Tag"),
    ] {
        docs_where(&mut tx, &env, field, operator, value)
            .await
            .unwrap();
    }
    let rid = |id: i64| erp_sdk::RecId::from_real(id).unwrap();
    tx.env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .browse([DEMO])
        .unwrap()
        .write(
            &mut tx,
            erp_sdk::RawVals::new()
                .set_commands("company_ids", [erp_sdk::Command::unlink(rid(w.o))]),
        )
        .await
        .unwrap();
    for (field, operator, value) in [
        ("mid_name", erp_sdk::Operator::Eq, "MidO"),
        ("tag_rel_ids", erp_sdk::Operator::ILike, "Tag"),
    ] {
        let err = docs_where(&mut tx, &env, field, operator, value)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{field}: {err}");
        assert_eq!(
            err.message(),
            "Access to unauthorized or invalid companies.",
            "{field}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SF1 (b): an environment made by `with_user` names no companies; the subject's default
/// companies bind the search, as they bind the read. Before the fix the search bound an empty
/// list and hid every document behind a company-scoped hop.
#[tokio::test]
async fn search_of_a_with_user_environment_binds_the_default_companies() {
    let h = resolver_harness().await;
    let w = world(&h).await;
    enrol(&h, &[w.r, w.o]).await;
    let mut tx = h.begin().await.unwrap();
    let tags = create(
        &mut tx,
        TAG,
        &[&[("name", text("TagOwn")), ("company_id", int(w.r))]],
    )
    .await;
    let mids = create(
        &mut tx,
        MID,
        &[&[
            ("name", text("TaggedMid")),
            ("company_id", int(w.r)),
            ("tag_ids", ids(&tags.ids())),
        ]],
    )
    .await;
    let tagged = create(
        &mut tx,
        DOC,
        &[&[("name", text("tagged")), ("mid_id", int(mids.ids()[0]))]],
    )
    .await
    .ids()[0];
    let env = tx.env().with_user(DEMO);
    assert_eq!(
        docs_where(&mut tx, &env, "mid_name", erp_sdk::Operator::Eq, "MidR")
            .await
            .unwrap(),
        [w.own]
    );
    assert_eq!(
        docs_where(&mut tx, &env, "mid_name", erp_sdk::Operator::Eq, "MidO")
            .await
            .unwrap(),
        [w.mid_foreign]
    );
    assert_eq!(
        docs_where(
            &mut tx,
            &env,
            "tag_rel_ids",
            erp_sdk::Operator::ILike,
            "TagOwn"
        )
        .await
        .unwrap(),
        [tagged]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
