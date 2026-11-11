//! the policy loaded from the database, a user's groups resolved per transaction.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_addon_base::ir_actions::{Bindings, get_bindings};
use erp_fields::{Cell, ErrorKind, Value};
use erp_orm::{Orm, PolicyProvider, RawVals, Transaction, UserId};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;
use support::{connect, pool};

static COUNTRY: AddonDescriptor = AddonDescriptor {
    name: "x_country",
    depends: &["base"],
    exact_only: false,
};

const NAME: &[FieldDecl] = &[FieldDecl::char("name")];

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &COUNTRY,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("x_country", "x.country")
            .description("Country")
            .fields(NAME),
        ModelDecl::new("x_country", "x.city")
            .description("City")
            .fields(NAME),
        ModelDecl::new("x_country", "x.hidden")
            .description("Hidden")
            .fields(NAME),
    ])
    .build()
    .await
    .expect("harness")
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

/// What the tests need of `base`: the internal user group, a group the demo user is not in,
/// `base.group_system` (the `code` field's group), `base.group_no_one`; the demo user, internal;
/// the reflection rows of the test models; read access for internal users on `x.country` and
/// `x.city`, none on `x.hidden`.
struct Seeded {
    demo: i64,
    restricted: i64,
    no_one: i64,
    country: i64,
    city: i64,
}

async fn seed(client: &Client) -> Seeded {
    let group = async |name: &str, xmlid: &str| -> i64 {
        let id = one(
            client,
            "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
            &[&name],
        )
        .await;
        let (module, name) = xmlid.split_once('.').unwrap();
        client
            .execute(
                "INSERT INTO ir_model_data (module, name, model, res_id) \
                 VALUES ($1, $2, 'res.groups', $3)",
                &[&module, &name, &id],
            )
            .await
            .unwrap();
        id
    };
    let internal = group("Internal User", "base.group_user").await;
    let restricted = group("Restricted", "x_country.group_restricted").await;
    group("Settings", "base.group_system").await;
    let no_one = group("Technical Features", "base.group_no_one").await;
    let demo = one(
        client,
        "INSERT INTO res_users (name, login, active, share) VALUES ('Demo', 'demo', true, false) \
         RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2), ($1, $3)",
            &[&demo, &internal, &no_one],
        )
        .await
        .unwrap();
    let model = async |name: &str, model: &str| -> i64 {
        one(
            client,
            "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $2, 'base', false) \
             RETURNING id::int8",
            &[&name, &model],
        )
        .await
    };
    let country = model("Country", "x.country").await;
    let city = model("City", "x.city").await;
    model("Hidden", "x.hidden").await;
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) VALUES \
             ('country read', $1, $3, 'r', NULL, true), ('city read', $2, $3, 'r', NULL, true)",
            &[&country, &city, &internal],
        )
        .await
        .unwrap();
    Seeded {
        demo,
        restricted,
        no_one,
        country,
        city,
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

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
}

fn vals(pairs: &[(&'static str, Cell)]) -> RawVals {
    pairs
        .iter()
        .fold(RawVals::new(), |v, (k, c)| v.set_cell(*k, c.clone()))
}

async fn create(tx: &mut Transaction, model: &str, v: RawVals) -> i64 {
    let records = tx.env().records(model).unwrap();
    records.create(tx, vec![v]).await.unwrap().ids()[0]
}

/// `(id, name, sequence)` of the `action` bindings, and the keys of each.
fn actions(bindings: &Bindings) -> Vec<(i64, Value, Option<Value>, Vec<String>)> {
    bindings
        .action()
        .iter()
        .map(|b| {
            let values = b.values();
            (
                b.id().get(),
                values.get("name").cloned().unwrap_or(Value::False),
                values.get("sequence").cloned(),
                values.keys().cloned().collect(),
            )
        })
        .collect()
}

async fn bindings_of(orm: &Orm, uid: i64, model: &str, debug: bool) -> Bindings {
    let mut tx = orm.begin(UserId::new(uid).unwrap()).await.unwrap();
    let env = tx.env().with_debug(debug);
    let bindings = get_bindings(&mut tx, &env, model).await.unwrap();
    tx.commit().await.unwrap();
    bindings
}

/// Two server actions bound to the country model with sequences 5 and 1, the second restricted
/// to a group the demo user lacks (`test_50_groups`, `test_60_sort`): the demo user gets only
/// the first until joining the group, then both in sequence order `[1, 5]`; each binding has
/// exactly `id, name, binding_view_types, sequence` (groups and `res_model` are left out). Window
/// actions bound to the city model are shown only when the user may read their `res_model`; a
/// `report` binding comes in its own list; a binding limited to `base.group_no_one` shows only
/// in debug mode. Membership changes need no cache invalidation: the shared part is computed
/// once and filtered per call.
#[tokio::test]
async fn bindings_by_groups_access_sequence() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;

    // the actions, written as the superuser
    let mut tx = h.begin().await.unwrap();
    let open = create(
        &mut tx,
        "ir.actions.server",
        vals(&[
            ("name", s("TestAction")),
            ("model_id", Cell::Id(seeded.country)),
            ("binding_model_id", Cell::Id(seeded.country)),
            ("state", s("code")),
            ("sequence", Cell::Int(5)),
        ]),
    )
    .await;
    let restricted = create(
        &mut tx,
        "ir.actions.server",
        vals(&[
            ("name", s("TestAction2")),
            ("model_id", Cell::Id(seeded.country)),
            ("binding_model_id", Cell::Id(seeded.country)),
            ("state", s("code")),
            ("sequence", Cell::Int(1)),
        ])
        .set_commands(
            "group_ids",
            [erp_orm::Command::link(
                erp_fields::RecId::from_real(seeded.restricted).unwrap(),
            )],
        ),
    )
    .await;
    let report = create(
        &mut tx,
        "ir.actions.act_url",
        vals(&[
            ("name", s("Print")),
            ("url", s("/report")),
            ("binding_model_id", Cell::Id(seeded.country)),
            ("binding_type", s("report")),
        ]),
    )
    .await;
    let readable = create(
        &mut tx,
        "ir.actions.act_window",
        vals(&[
            ("name", s("Countries")),
            ("res_model", s("x.country")),
            ("binding_model_id", Cell::Id(seeded.city)),
            ("domain", s("[('name', '!=', False)]")),
        ]),
    )
    .await;
    create(
        &mut tx,
        "ir.actions.act_window",
        vals(&[
            ("name", s("Hidden records")),
            ("res_model", s("x.hidden")),
            ("binding_model_id", Cell::Id(seeded.city)),
            ("domain", s("")),
        ]),
    )
    .await;
    // review C-R3: a binding the user may read whose domain is empty has no `domain` key
    let readable_empty = create(
        &mut tx,
        "ir.actions.act_window",
        vals(&[
            ("name", s("Countries, all")),
            ("res_model", s("x.country")),
            ("binding_model_id", Cell::Id(seeded.city)),
            ("domain", s("")),
        ]),
    )
    .await;
    let technical = create(
        &mut tx,
        "ir.actions.act_url",
        vals(&[
            ("name", s("Technical")),
            ("url", s("/debug")),
            ("binding_model_id", Cell::Id(seeded.city)),
        ]),
    )
    .await;
    // a window action limited to `base.group_no_one`
    let debug_only = create(
        &mut tx,
        "ir.actions.act_window",
        vals(&[
            ("name", s("Debug only")),
            ("res_model", s("x.city")),
            ("binding_model_id", Cell::Id(seeded.city)),
        ])
        .set_commands(
            "group_ids",
            [erp_orm::Command::link(
                erp_fields::RecId::from_real(seeded.no_one).unwrap(),
            )],
        ),
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);

    let orm = policy_orm(&h).await;
    let keys = |with_sequence: bool| -> Vec<String> {
        let mut k = vec!["id", "name", "binding_view_types"];
        if with_sequence {
            k.push("sequence");
        }
        k.into_iter().map(str::to_owned).collect()
    };
    let before = bindings_of(&orm, seeded.demo, "x.country", false).await;
    assert_eq!(
        actions(&before),
        vec![(
            open,
            Value::str("TestAction"),
            Some(Value::Int(5)),
            keys(true)
        )]
    );
    assert_eq!(
        before
            .report()
            .iter()
            .map(|b| (b.id().get(), b.values().keys().cloned().collect::<Vec<_>>()))
            .collect::<Vec<_>>(),
        vec![(report, keys(false))]
    );
    assert_eq!(
        before.action()[0].values().get("binding_view_types"),
        Some(&Value::str("list,form"))
    );

    // joining the group shows the restricted action, first by sequence
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&seeded.demo, &seeded.restricted],
        )
        .await
        .unwrap();
    let after = bindings_of(&orm, seeded.demo, "x.country", false).await;
    assert_eq!(
        actions(&after),
        vec![
            (
                restricted,
                Value::str("TestAction2"),
                Some(Value::Int(1)),
                keys(true)
            ),
            (
                open,
                Value::str("TestAction"),
                Some(Value::Int(5)),
                keys(true)
            ),
        ]
    );
    let tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    assert_eq!(
        tx.ormcache_stats("ir.actions.actions", "_get_bindings"),
        (1, 1),
        "the shared part was computed once and reused across the membership change"
    );
    drop(tx);

    // the city's bindings: the readable window action with its domain, not the one on
    // x.hidden (whose empty domain would be dropped), the url action (no groups, no res_model),
    // and the debug-group window action only in debug mode
    let city = bindings_of(&orm, seeded.demo, "x.city", false).await;
    let ids: Vec<i64> = city.action().iter().map(|b| b.id().get()).collect();
    assert_eq!(ids, vec![readable, readable_empty, technical]);
    assert_eq!(
        city.action()[1]
            .values()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["id", "name", "binding_view_types"]
    );
    assert_eq!(
        city.action()[0].values().get("domain"),
        Some(&Value::str("[('name', '!=', False)]"))
    );
    assert_eq!(
        city.action()[0]
            .values()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["id", "name", "binding_view_types", "domain"]
    );
    let debug = bindings_of(&orm, seeded.demo, "x.city", true).await;
    let ids: Vec<i64> = debug.action().iter().map(|b| b.id().get()).collect();
    assert_eq!(ids, vec![readable, readable_empty, technical, debug_only]);

    // a model the registry does not have is an error, not an empty answer
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let error = get_bindings(&mut tx, &env, "x.nothing").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "get_bindings: there is no model 'x.nothing' in the registry"
    );
    drop(tx);

    // an action write invalidates the shared part (in this process; across processes: MN-4)
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("ir.actions.server")
        .unwrap()
        .browse([open])
        .unwrap()
        .write(&mut tx, vals(&[("sequence", Cell::Int(0))]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let resorted = bindings_of(&orm, seeded.demo, "x.country", false).await;
    let ids: Vec<i64> = resorted.action().iter().map(|b| b.id().get()).collect();
    assert_eq!(ids, vec![open, restricted]);
    h.cleanup().await.unwrap();
}

/// Review C-R1: a group removed through the ORM takes its relation rows with it (the foreign keys
/// cascade), so an action restricted to it only is restricted no more; the bindings cache follows
/// in the removing transaction (the ORM follows the cascade into the action's group relation),
/// so the demo user sees the action at once, without waiting for another action write.
#[tokio::test]
async fn a_removed_group_frees_its_bindings() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let gone = one(
        &client,
        "INSERT INTO res_groups (name) VALUES ('Short-lived') RETURNING id::int8",
        &[],
    )
    .await;
    // the record rules of res.groups name the regular internal user group
    let regular = one(
        &client,
        "INSERT INTO res_groups (name) VALUES ('Role / User') RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', 'group_user_regular', 'res.groups', $1)",
            &[&regular],
        )
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let action = create(
        &mut tx,
        "ir.actions.server",
        vals(&[
            ("name", s("Restricted")),
            ("model_id", Cell::Id(seeded.country)),
            ("binding_model_id", Cell::Id(seeded.country)),
            ("state", s("code")),
        ])
        .set_commands(
            "group_ids",
            [erp_orm::Command::link(
                erp_fields::RecId::from_real(gone).unwrap(),
            )],
        ),
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);

    let orm = policy_orm(&h).await;
    let ids = |b: &Bindings| b.action().iter().map(|b| b.id().get()).collect::<Vec<_>>();
    assert_eq!(
        ids(&bindings_of(&orm, seeded.demo, "x.country", false).await),
        Vec::<i64>::new(),
        "not in the group"
    );
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("res.groups")
        .unwrap()
        .browse([gone])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        ids(&bindings_of(&orm, seeded.demo, "x.country", false).await),
        vec![action],
        "no groups left: everybody"
    );
    let tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    assert_eq!(
        tx.ormcache_stats("ir.actions.actions", "_get_bindings"),
        (0, 2),
        "computed again after the removal"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
