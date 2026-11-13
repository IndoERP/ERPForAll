// SPDX-License-Identifier: MIT OR Apache-2.0
//! The activity menu and the selected companies: a
//! record that only the company switch hides is left out of every group, a record hidden for any
//! other reason (here a name condition, or a company the user does not belong to) counts under
//! "Other Activities".
//!
//! The test lives here and not in `addons/mail/tests/activity.rs` because a user who belongs to
//! several companies is built with `erp-orm`'s `PrincipalBuilder`, which an addon crate may not
//! use (`scripts/check-layers.py`). The read rule of the document model is a `DomainPolicy`
//! predicate: `company_id in company_ids` and a name that is not `secret`. Every compared value
//! is an id the test created or a figure the menu returned.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::num::NonZeroU64;

use erp_addon_mail::{ActivityGroups, MailActivityMixin, MailThread, activity_groups};
use erp_orm::{
    BeginOptions, CompanyId, Context, GroupSet, Principal, PrincipalBuilder, Resolution, UserId,
};
use erp_sdk::Operation;
use erp_sdk::domain::PrincipalParam;
use erp_sdk::prelude::*;
use erp_testkit::fixtures::seed_currency_in;
use erp_testkit::{DomainPolicy, Harness, Otherwise};

erp_sdk::addon!(name = "test_menu_company", depends = ["mail"]);

/// A document with chatter, activities and a company.
#[model(
    name = "test_menu_company.doc",
    description = "Menu Company Doc",
    inherit = [MailThread, MailActivityMixin],
    addon = "test_menu_company",
    company_access = unrestricted(reason = "company fixture; the read rule of the test policy is what is under test")
)]
pub struct Doc {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

const DOC: &str = "test_menu_company.doc";
const USER: i64 = 42;

async fn harness() -> Harness {
    let in_selected = Domain::condition(
        "company_id",
        Operator::In,
        erp_sdk::domain::Value::Param(PrincipalParam::CompanyIds),
    )
    .unwrap();
    let not_secret = Domain::condition("name", Operator::Ne, "secret").unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(DOC, Operation::Read, Domain::and([in_selected, not_secret]))
        .build()
        .unwrap();
    // `ir.model.data` is declared by `erp-data`: the subtype and type lookups of an activity need it
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_addon_mail::ADDON,
        &ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .policy(policy)
    .build()
    .await
    .unwrap()
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> i64 {
    let records = tx.env().records(model).unwrap();
    records.create(tx, vec![vals]).await.unwrap().ids()[0]
}

async fn xmlid(tx: &mut Transaction, model: &str, name: &str, id: i64) {
    create(
        tx,
        "ir.model.data",
        RawVals::new()
            .set_cell("module", text("mail"))
            .set_cell("name", text(name))
            .set_cell("model", text(model))
            .set_cell("res_id", Cell::Id(id)),
    )
    .await;
}

/// What the harness does not load from data files and an activity needs: the subtypes and the
/// To-Do type with their external ids, a partner for the superuser, and the user of the menu.
async fn seed(tx: &mut Transaction) {
    for (name, internal, default) in [
        ("mt_note", true, false),
        ("mt_comment", false, true),
        ("mt_activities", true, false),
    ] {
        let id = create(
            tx,
            "mail.message.subtype",
            RawVals::new()
                .set_cell("name", text(name))
                .set_cell("internal", Cell::Bool(internal))
                .set_cell("default", Cell::Bool(default)),
        )
        .await;
        xmlid(tx, "mail.message.subtype", name, id).await;
    }
    let todo = create(
        tx,
        "mail.activity.type",
        RawVals::new()
            .set_cell("name", text("To-Do"))
            .set_cell("summary", text("To-Do"))
            .set_cell("sequence", Cell::Int(2))
            .set_cell("delay_count", Cell::Int(5)),
    )
    .await;
    xmlid(tx, "mail.activity.type", "mail_activity_data_todo", todo).await;
    let root = create(
        tx,
        "res.partner",
        RawVals::new().set_cell("name", text("root")),
    )
    .await;
    let me = create(
        tx,
        "res.partner",
        RawVals::new().set_cell("name", text("me")),
    )
    .await;
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE res_users SET partner_id = $2 WHERE id = $1",
        &[&1_i64, &root],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active, share, partner_id) \
         VALUES ($1, 'me', 'me', true, false, $2)",
        &[&USER, &me],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
}

async fn company(tx: &mut Transaction, currency: i64, name: &str) -> i64 {
    let rows = tx
        .query(
            "INSERT INTO res_company (name, currency_id, active) VALUES ($1, $2, true) \
             RETURNING id::int8",
            &[&name, &currency],
        )
        .await
        .unwrap();
    rows[0].try_get(0).unwrap()
}

/// A document of `company_id` named `name` and an open activity of the user on it, due today.
async fn doc_with_activity(tx: &mut Transaction, name: &str, company_id: i64) -> i64 {
    let doc = create(
        tx,
        DOC,
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("company_id", Cell::Id(company_id)),
    )
    .await;
    let vals = RawVals::new()
        .set_cell("res_model", text(DOC))
        .set_cell("res_id", Cell::Id(doc))
        .set_cell("user_id", Cell::Id(USER))
        .set_cell("date_deadline", Cell::Date(Date::today()));
    create_activity(tx, vals).await
}

async fn create_activity(tx: &mut Transaction, vals: RawVals) -> i64 {
    let env = tx.env();
    env.records("mail.activity")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

fn company_id(id: i64) -> CompanyId {
    CompanyId::new(id).unwrap()
}

/// The user, member of `member_of` (the first is the main company).
fn principal(member_of: &[i64]) -> Principal {
    let resolution = Resolution::in_id_order(
        GroupSet::default(),
        member_of.iter().map(|c| company_id(*c)),
        NonZeroU64::MIN,
    )
    .unwrap()
    .with_default_order(member_of.iter().map(|c| company_id(*c)).collect())
    .unwrap();
    PrincipalBuilder::user(UserId::new(USER).unwrap())
        .resolution(resolution)
        .companies(member_of.iter().map(|c| company_id(*c)).collect())
        .build()
        .unwrap()
}

/// The menu of the user who belongs to `member_of` and works in `selected`.
async fn menu(h: &Harness, member_of: &[i64], selected: &[i64]) -> ActivityGroups {
    let context = Context::from_pairs([(
        "allowed_company_ids",
        Value::list(selected.iter().map(|c| Value::Int(*c))),
    )]);
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(member_of)).context(context))
        .await
        .unwrap();
    let env = tx.env();
    let groups = activity_groups(&env, &mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    groups
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids
}

/// The ids of the group of `model`, if the menu has one.
fn ids_of(menu: &ActivityGroups, model: &str) -> Option<Vec<i64>> {
    menu.groups()
        .iter()
        .find(|g| g.model() == model)
        .map(|g| g.activity_ids().to_vec())
}

/// Every counted activity id of the menu, in every group.
fn every_id(menu: &ActivityGroups) -> Vec<i64> {
    sorted(
        menu.groups()
            .iter()
            .flat_map(|g| g.activity_ids().iter().copied())
            .collect(),
    )
}

#[tokio::test]
async fn a_record_of_an_unselected_company_is_left_out_of_every_group() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let currency = seed_currency_in(&mut tx).await.unwrap();
    let c1 = company(&mut tx, currency, "C1").await;
    let c2 = company(&mut tx, currency, "C2").await;
    seed(&mut tx).await;
    let on_c1 = doc_with_activity(&mut tx, "one", c1).await;
    let on_c2 = doc_with_activity(&mut tx, "two", c2).await;
    tx.commit().await.unwrap();

    // (i) both companies selected: both records count in the model group, nothing else
    let both = menu(&h, &[c1, c2], &[c1, c2]).await;
    assert_eq!(ids_of(&both, DOC), Some(sorted(vec![on_c1, on_c2])));
    assert_eq!(ids_of(&both, "mail.activity"), None);
    assert_eq!(both.groups().len(), 1);
    assert_eq!(both.counter(), 2);

    // a record the rule hides for a reason that has nothing to do with companies
    let mut tx = h.begin().await.unwrap();
    let hidden = doc_with_activity(&mut tx, "secret", c1).await;
    tx.commit().await.unwrap();

    // (ii) only C1 selected: the C2 activity is in no group, the secret one is under Other
    let only_c1 = menu(&h, &[c1, c2], &[c1]).await;
    assert_eq!(ids_of(&only_c1, DOC), Some(vec![on_c1]));
    assert_eq!(ids_of(&only_c1, "mail.activity"), Some(vec![hidden]));
    assert!(!every_id(&only_c1).contains(&on_c2));
    assert_eq!(only_c1.groups().len(), 2);
    let other = only_c1.groups().last().unwrap();
    assert_eq!(other.model(), "mail.activity");
    assert_eq!(other.name(), "Other Activities");
    assert_eq!(only_c1.counter(), 2);

    // the same with only C2 selected (not the user's first company): now the C1 record is left out
    let only_c2 = menu(&h, &[c1, c2], &[c2]).await;
    assert_eq!(ids_of(&only_c2, DOC), Some(vec![on_c2]));
    assert_eq!(ids_of(&only_c2, "mail.activity"), Some(vec![hidden]));
    assert!(!every_id(&only_c2).contains(&on_c1));

    // (iii) C2 is not one of the user's companies any more: its record counts under Other
    let lost_c2 = menu(&h, &[c1], &[c1]).await;
    assert_eq!(ids_of(&lost_c2, DOC), Some(vec![on_c1]));
    assert_eq!(
        ids_of(&lost_c2, "mail.activity"),
        Some(sorted(vec![on_c2, hidden]))
    );
    assert_eq!(lost_c2.counter(), 3);

    h.cleanup().await.unwrap();
}
