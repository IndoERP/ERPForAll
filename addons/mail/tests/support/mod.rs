//! Shared fixtures of the `mail` tests: a native model inheriting the thread mixin and helpers to
//! create partners, followers and messages.
#![allow(dead_code, reason = "each test binary uses a subset")]

use erp_addon_mail::MailThread;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_mail", depends = ["mail"]);

/// A native model that inherits the thread mixin.
#[model(
    name = "test_mail.ticket",
    description = "Mail Test Ticket",
    inherit = [MailThread],
    addon = "test_mail"
)]
pub struct Ticket {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A second test addon with two more models that inherit the thread mixin: a registry that
/// includes it has more thread models than the plain one, to show that follower statements do not
/// depend on how many models inherit `mail.thread`.
pub mod more {
    use erp_addon_mail::MailThread;
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_mail_more", depends = ["mail"]);

    /// A thread model of the second addon.
    #[model(
        name = "test_mail_more.note",
        description = "Mail Test Note",
        inherit = [MailThread],
        addon = "test_mail_more"
    )]
    pub struct Note {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }

    /// A second thread model of the second addon.
    #[model(
        name = "test_mail_more.task",
        description = "Mail Test Task",
        inherit = [MailThread],
        addon = "test_mail_more"
    )]
    pub struct Task {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }
}

pub fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

pub async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> i64 {
    let records = tx.env().records(model).unwrap();
    records.create(tx, vec![vals]).await.unwrap().ids()[0]
}

pub async fn partner(tx: &mut Transaction, name: &str) -> i64 {
    create(
        tx,
        "res.partner",
        RawVals::new().set_cell("name", text(name)),
    )
    .await
}

pub async fn ticket(tx: &mut Transaction, name: &str) -> i64 {
    create(
        tx,
        "test_mail.ticket",
        RawVals::new().set_cell("name", text(name)),
    )
    .await
}

pub async fn subtype(tx: &mut Transaction, name: &str, vals: RawVals) -> i64 {
    create(
        tx,
        "mail.message.subtype",
        vals.set_cell("name", text(name)),
    )
    .await
}

pub async fn follower(
    tx: &mut Transaction,
    model: &str,
    res_id: i64,
    partner: i64,
    subtypes: &[i64],
) -> i64 {
    let ids: Vec<RecId> = subtypes
        .iter()
        .map(|id| RecId::from_real(*id).unwrap())
        .collect();
    create(
        tx,
        "mail.followers",
        RawVals::new()
            .set_cell("res_model", text(model))
            .set_cell("res_id", Cell::Id(res_id))
            .set_cell("partner_id", Cell::Id(partner))
            .set_commands("subtype_ids", [Command::set(ids)]),
    )
    .await
}

pub async fn message(tx: &mut Transaction, model: &str, res_id: i64, body: &str) -> i64 {
    create(
        tx,
        "mail.message",
        RawVals::new()
            .set_cell("model", text(model))
            .set_cell("res_id", Cell::Id(res_id))
            .set_cell("body", text(body)),
    )
    .await
}

/// Ids of `model` records matching `domain`, in id order.
pub async fn ids(tx: &mut Transaction, model: &str, domain: &Domain) -> Vec<i64> {
    let mut ids = tx
        .env()
        .records(model)
        .unwrap()
        .search(tx, domain)
        .await
        .unwrap()
        .ids();
    ids.sort_unstable();
    ids
}

/// Ids held by an x2many cell.
pub fn id_list(cell: Cell) -> Vec<i64> {
    match cell {
        Cell::Ids(ids) => {
            let mut ids: Vec<i64> = ids.iter().map(|id| id.get()).collect();
            ids.sort_unstable();
            ids
        }
        other => panic!("expected ids, got {other:?}"),
    }
}
