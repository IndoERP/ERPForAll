//! database, compiled (user paths, `child_of`), resolved for a portal user and lowered into SQL.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::{ErrorKind, IndexKind};
use erp_orm::{Orm, PolicyProvider, Transaction, UserId};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;
use support::{connect, pool};

static PORTAL: AddonDescriptor = AddonDescriptor {
    name: "x_portal",
    depends: &["base"],
    exact_only: false,
};

const PARTNER_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name").required(true),
    FieldDecl::many2one("parent_id", "x.partner"),
    FieldDecl::char("parent_path").index(Some(IndexKind::Btree)),
    FieldDecl::many2one("commercial_partner_id", "x.partner"),
];

const USER_FIELDS: &[FieldDecl] = &[
    FieldDecl::many2one("partner_id", "x.partner"),
    FieldDecl::many2one("commercial_partner_id", "x.partner"),
];

const TICKET_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::many2many("message_partner_ids", "x.partner"),
];

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &PORTAL,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("x_portal", "x.partner")
            .description("Contact")
            .fields(PARTNER_FIELDS)
            .parent_store(true),
        ModelDecl::extension("x_portal", "res.users", &["res.users"]).fields(USER_FIELDS),
        ModelDecl::new("x_portal", "x.ticket")
            .description("Ticket")
            .fields(TICKET_FIELDS),
    ])
    .build()
    .await
    .expect("harness")
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

/// Ids of what `seed` committed.
struct Seeded {
    paul: i64,
    mine: i64,
    my_company: i64,
    others: i64,
    company: i64,
    paul_partner: i64,
    colleague: i64,
    outsider: i64,
}

async fn seed(client: &Client) -> Seeded {
    let internal = one(
        client,
        "INSERT INTO res_groups (name) VALUES ('Internal User') RETURNING id::int8",
        &[],
    )
    .await;
    let portal = one(
        client,
        "INSERT INTO res_groups (name, share) VALUES ('Portal', true) RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', 'group_user', 'res.groups', $1), \
                    ('base', 'group_portal', 'res.groups', $2)",
            &[&internal, &portal],
        )
        .await
        .unwrap();
    let company = one(
        client,
        "INSERT INTO x_partner (name, parent_path) VALUES ('Paul Inc', '') RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "UPDATE x_partner SET parent_path = id || '/', commercial_partner_id = id WHERE id = $1",
            &[&company],
        )
        .await
        .unwrap();
    let contact = async |name: &str| -> i64 {
        let id = one(
            client,
            "INSERT INTO x_partner (name, parent_id, commercial_partner_id, parent_path) \
             VALUES ($1, $2, $2, '') RETURNING id::int8",
            &[&name, &company],
        )
        .await;
        client
            .execute(
                "UPDATE x_partner SET parent_path = $2 || '/' || id || '/' WHERE id = $1",
                &[&id, &company.to_string()],
            )
            .await
            .unwrap();
        id
    };
    let paul_partner = contact("Paul").await;
    let colleague = contact("Colleague").await;
    let outsider = one(
        client,
        "INSERT INTO x_partner (name, parent_path) VALUES ('Internal person', '') RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "UPDATE x_partner SET parent_path = id || '/', commercial_partner_id = id WHERE id = $1",
            &[&outsider],
        )
        .await
        .unwrap();
    let paul = one(
        client,
        "INSERT INTO res_users (name, login, active, share, partner_id, commercial_partner_id) \
         VALUES ('Paul', 'paul', true, true, $1, $2) RETURNING id::int8",
        &[&paul_partner, &company],
    )
    .await;
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&paul, &portal],
        )
        .await
        .unwrap();
    let model = one(
        client,
        "INSERT INTO ir_model (name, model, state, transient) \
         VALUES ('Ticket', 'x.ticket', 'base', false) RETURNING id::int8",
        &[],
    )
    .await;
    let partner_model = one(
        client,
        "INSERT INTO ir_model (name, model, state, transient) \
         VALUES ('Contact', 'x.partner', 'base', false) RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) VALUES \
             ('Fake Ticket: portal or following', $1, $2, 'r', \
              '[''|'', \
                (''message_partner_ids'', ''child_of'', [user.partner_id.commercial_partner_id.id]), \
                (''message_partner_ids'', ''in'', [user.partner_id.id])]', true), \
             ('access_test_ticket_user', $1, $3, 'crud', NULL, true), \
             ('res_partner: portal/public: read access on my commercial partner', $4, $2, 'r', \
              '[(''id'', ''child_of'', user.commercial_partner_id.id)]', true)",
            &[&model, &portal, &internal, &partner_model],
        )
        .await
        .unwrap();
    let ticket = async |name: &str, partners: &[i64]| -> i64 {
        let id = one(
            client,
            "INSERT INTO x_ticket (name) VALUES ($1) RETURNING id::int8",
            &[&name],
        )
        .await;
        for partner in partners {
            client
                .execute(
                    "INSERT INTO x_partner_x_ticket_rel (x_ticket_id, x_partner_id) \
                     VALUES ($1, $2)",
                    &[&id, partner],
                )
                .await
                .unwrap();
        }
        id
    };
    let mine = ticket("Need help here", &[paul_partner, outsider]).await;
    let my_company = ticket("For the colleague", &[colleague]).await;
    let others = ticket("Internal only", &[outsider]).await;
    Seeded {
        paul,
        mine,
        my_company,
        others,
        company,
        paul_partner,
        colleague,
        outsider,
    }
}

async fn policy_orm(h: &Harness) -> Orm {
    let source = PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(h.registry()), pool(h, 2))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap()
}

fn refused(paul: i64, label: &str) -> String {
    format!(
        "Access rules protect some of these records.\n\nPaul (id={paul}) has no 'read' access to:\n- {label}\n\nAsk an administrator if you need access to these records."
    )
}

async fn search(tx: &mut Transaction, model: &str) -> erp_fields::Result<Vec<i64>> {
    let records = tx.env().records(model)?;
    let mut ids = records.search(tx, &Domain::constant(true)).await?.ids();
    ids.sort_unstable();
    Ok(ids)
}

/// `has_group` on another user is.
#[tokio::test]
async fn portal_users_see_the_tickets_of_their_commercial_partner() {
    let h = harness().await;
    let client = connect(&h).await;
    let s = seed(&client).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(s.paul).unwrap()).await.unwrap();

    assert_eq!(
        search(&mut tx, "x.ticket").await.unwrap(),
        [s.mine, s.my_company]
    );

    // reading the ticket is not blocked by the partners it points to (test_check_access)
    let ticket = tx
        .env()
        .records("x.ticket")
        .unwrap()
        .browse([s.mine])
        .unwrap();
    ticket
        .check_access(&mut tx, erp_orm::Operation::Read)
        .await
        .unwrap();
    let name = ticket.value(&mut tx, "name").await.unwrap();
    assert_eq!(name, erp_fields::Value::str("Need help here"));

    // the internal ticket is a record-level refusal
    let other = tx
        .env()
        .records("x.ticket")
        .unwrap()
        .browse([s.others])
        .unwrap();
    let err = other.value(&mut tx, "name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), refused(s.paul, "Ticket (x.ticket)"));

    // the partners of his commercial partner, not the internal person
    assert_eq!(
        search(&mut tx, "x.partner").await.unwrap(),
        [s.company, s.paul_partner, s.colleague]
    );
    let outsider = tx
        .env()
        .records("x.partner")
        .unwrap()
        .browse([s.outsider])
        .unwrap();
    let err = outsider.value(&mut tx, "name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), refused(s.paul, "Contact (x.partner)"));
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}
