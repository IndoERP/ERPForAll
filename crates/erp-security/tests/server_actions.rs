//! database, the users' groups resolved per transaction), as the caller.
//! Two fixture addons (`test_ext_a`, `test_ext_b`) each add a server
//! action type by `selection_add` with its runner. Separate tests preserve that extension seam;
//! `test_40_multi` now runs the real Luau `code` children.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

#[path = "server_actions/context_path.rs"]
mod context_path;
#[path = "server_actions/mutation_cases.rs"]
mod mutation_cases;
#[path = "server_actions/payload_cases.rs"]
mod payload_cases;
#[path = "server_actions/script_cases.rs"]
mod script_cases;

use std::str::FromStr;
use std::sync::Arc;

use erp_addon_base::ir_actions::ActionId;
use erp_addon_base::server_action::{
    ActionResult, MAX_ACTIONS, MAX_DEPTH, RunLimit, RunRequest, RunnerCtx, ServerActionError,
    check_runners, run_server_action,
};
use erp_fields::serde_json::json;
use erp_fields::{Cell, Error, ErrorKind, RealId, RecId, Value};
use erp_orm::{BoxFuture, Command, Orm, PolicyProvider, RawVals, Transaction, UserId};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;
use support::{connect, pool};

// ----- fixture addons ------------------------------------------------------------------------------

static PARTNER: AddonDescriptor = AddonDescriptor {
    name: "x_partner",
    depends: &["base"],
    exact_only: false,
};
static EXT_A: AddonDescriptor = AddonDescriptor {
    name: "test_ext_a",
    depends: &["base"],
    exact_only: false,
};
static EXT_B: AddonDescriptor = AddonDescriptor {
    name: "test_ext_b",
    depends: &["base"],
    exact_only: false,
};
/// Adds a state without a runner.
static EXT_NORUNNER: AddonDescriptor = AddonDescriptor {
    name: "test_ext_norunner",
    depends: &["base"],
    exact_only: false,
};
/// Declares a second runner for `test_ext_a`'s state.
static EXT_DUP: AddonDescriptor = AddonDescriptor {
    name: "test_ext_dup",
    depends: &["base", "test_ext_a"],
    exact_only: false,
};
/// Declares a runner for a state the selection does not have.
static EXT_ORPHAN: AddonDescriptor = AddonDescriptor {
    name: "test_ext_orphan",
    depends: &["base"],
    exact_only: false,
};

const COUNTRY_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name").required(true),
    FieldDecl::char("code"),
];
const TAG_FIELDS: &[FieldDecl] = &[FieldDecl::char("name")];
const PARTNER_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name").required(true),
    FieldDecl::char("city"),
    FieldDecl::many2one("country_id", "x.country"),
    FieldDecl::many2one("parent_id", "x.partner"),
    FieldDecl::one2many("child_ids", "x.partner", "parent_id"),
    FieldDecl::many2many("tag_ids", "x.tag"),
    FieldDecl::integer("count"),
    FieldDecl::float("ratio"),
    FieldDecl::boolean("flag"),
    FieldDecl::text("note"),
    FieldDecl::char("script_secret").groups("base.group_system"),
    FieldDecl::json("data"),
    FieldDecl::date("day"),
    FieldDecl::datetime("stamp"),
    FieldDecl::float_digits(
        "amount",
        erp_fields::Digits::Fixed {
            precision: 16,
            scale: 2,
        },
    ),
    FieldDecl::html("body"),
];
const NONAME_FIELDS: &[FieldDecl] = &[FieldDecl::char("code")];
/// A model whose `_rec_name` is an html field.
const PAGE_FIELDS: &[FieldDecl] = &[FieldDecl::html("name")];
const STATE_A: &[FieldDecl] = &[FieldDecl::selection_add_ondelete(
    "state",
    &[("x_open_window", Some("Open Window"))],
    &[(
        "x_open_window",
        erp_registry::SelectionOnDeleteDecl::Cascade,
    )],
)];
const STATE_B: &[FieldDecl] = &[FieldDecl::selection_add_ondelete(
    "state",
    &[("x_open_url", Some("Open URL"))],
    &[("x_open_url", erp_registry::SelectionOnDeleteDecl::Cascade)],
)];
const STATE_NORUNNER: &[FieldDecl] = &[FieldDecl::selection_add_ondelete(
    "state",
    &[("x_norunner", Some("No Runner"))],
    &[("x_norunner", erp_registry::SelectionOnDeleteDecl::Cascade)],
)];

fn open_window(_ctx: RunnerCtx<'_>) -> BoxFuture<'_, erp_fields::Result<Option<ActionResult>>> {
    Box::pin(async move {
        Ok(Some(ActionResult::new(
            json!({"type": "ir.actions.act_window"}),
        )?))
    })
}

/// Per record: the act_url of the active record.
fn open_url(ctx: RunnerCtx<'_>) -> BoxFuture<'_, erp_fields::Result<Option<ActionResult>>> {
    Box::pin(async move {
        let ids = ctx.records().ids();
        let active_id = ctx.env().context().get("active_id");
        assert_eq!(active_id, Some(Value::Int(ids[0])), "active_id in context");
        Ok(Some(ActionResult::new(json!({
            "type": "ir.actions.act_url",
            "url": format!("/record/{}", ids[0]),
        }))?))
    })
}

/// A runner that returns what is not an action.
fn never(_ctx: RunnerCtx<'_>) -> BoxFuture<'_, erp_fields::Result<Option<ActionResult>>> {
    Box::pin(async move { Ok(Some(ActionResult::new(json!({"url": "/"}))?)) })
}

erp_addon_base::server_action_runner!(
    EXT_A_RUNNER = { addon: EXT_A, state: "x_open_window", multi: true, run: open_window }
);
erp_addon_base::server_action_runner!(
    EXT_B_RUNNER = { addon: EXT_B, state: "x_open_url", multi: false, run: open_url }
);
erp_addon_base::server_action_runner!(
    EXT_DUP_RUNNER = { addon: EXT_DUP, state: "x_open_window", multi: true, run: never }
);
erp_addon_base::server_action_runner!(
    EXT_ORPHAN_RUNNER = { addon: EXT_ORPHAN, state: "x_orphan", multi: true, run: never }
);

fn decls() -> Vec<ModelDecl> {
    vec![
        erp_data::ir_model_data_decl(),
        ModelDecl::new("x_partner", "x.country")
            .description("Country")
            .fields(COUNTRY_FIELDS),
        ModelDecl::new("x_partner", "x.tag")
            .description("Tag")
            .fields(TAG_FIELDS),
        ModelDecl::new("x_partner", "x.partner")
            .description("Contact")
            .fields(PARTNER_FIELDS),
        ModelDecl::new("x_partner", "x.noname")
            .description("No name")
            .fields(NONAME_FIELDS),
        ModelDecl::new("x_partner", "x.page")
            .description("Page")
            .fields(PAGE_FIELDS),
        ModelDecl::extension("test_ext_a", "ir.actions.server", &["ir.actions.server"])
            .fields(STATE_A),
        ModelDecl::extension("test_ext_b", "ir.actions.server", &["ir.actions.server"])
            .fields(STATE_B),
        ModelDecl::extension(
            "test_ext_norunner",
            "ir.actions.server",
            &["ir.actions.server"],
        )
        .fields(STATE_NORUNNER),
    ]
}

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &PARTNER,
        &EXT_A,
        &EXT_B,
        &EXT_NORUNNER,
    ])
    .with_decls(decls())
    .build()
    .await
    .expect("harness")
}

// ----- seed ----------------------------------------------------------------------------------------

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

/// Ids of what `seed` committed.
struct Seeded {
    /// Internal user: reads everything, writes partners not named `Locked`, reads countries.
    demo: i64,
    /// Internal user in the action group `runner`.
    runner: i64,
    /// Internal user and administrator (`base.group_system`).
    admin: i64,
    group_runner: i64,
    partner_model: i64,
    country_model: i64,
    tag_model: i64,
    noname_model: i64,
    page_model: i64,
    /// `ir.model.fields` rows of x.partner.
    parent_field: i64,
    children_field: i64,
    tags_field: i64,
    city_field: i64,
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
    let system = group("Settings", "base.group_system").await;
    let group_runner = group("Runners", "x_partner.group_runner").await;
    let user = async |login: &str, groups: &[i64]| -> i64 {
        let id = one(
            client,
            "INSERT INTO res_users (name, login, active, share) VALUES ($1, $1, true, false) \
             RETURNING id::int8",
            &[&login],
        )
        .await;
        for g in groups {
            client
                .execute(
                    "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
                    &[&id, g],
                )
                .await
                .unwrap();
        }
        id
    };
    let demo = user("demo", &[internal]).await;
    let runner = user("runner", &[internal, group_runner]).await;
    let admin = user("admin", &[internal, system]).await;
    let model = async |name: &str, model: &str| -> i64 {
        one(
            client,
            "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $2, 'base', false) \
             RETURNING id::int8",
            &[&name, &model],
        )
        .await
    };
    let partner_model = model("Contact", "x.partner").await;
    let country_model = model("Country", "x.country").await;
    let tag_model = model("Tag", "x.tag").await;
    let noname_model = model("No name", "x.noname").await;
    let page_model = model("Page", "x.page").await;
    let actions_model = model("Server Actions", "ir.actions.server").await;
    let groups_model = model("Access Groups", "res.groups").await;
    let field = async |name: &str, ttype: &str, relation: Option<&str>| -> i64 {
        one(
            client,
            "INSERT INTO ir_model_fields (name, model_id, model, ttype, relation, state) \
             VALUES ($1, $2, 'x.partner', $3, $4, 'base') RETURNING id::int8",
            &[&name, &partner_model, &ttype, &relation],
        )
        .await
    };
    let parent_field = field("parent_id", "many2one", Some("x.partner")).await;
    let children_field = field("child_ids", "one2many", Some("x.partner")).await;
    let tags_field = field("tag_ids", "many2many", Some("x.tag")).await;
    let city_field = field("city", "char", None).await;
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) VALUES \
             ('partner read', $1, $4, 'r', NULL, true), \
             ('partner write', $1, $4, 'cud', '[(''name'', ''!='', ''Locked'')]', true), \
             ('country read', $2, $4, 'r', NULL, true), \
             ('tag create', $3, $4, 'cr', NULL, true), \
             ('actions', $5, $4, 'crud', NULL, true), \
             ('no name', $6, $4, 'crud', NULL, true), \
             ('groups read', $7, $4, 'r', NULL, true), \
             ('page', $8, $4, 'crud', NULL, true)",
            &[
                &partner_model,
                &country_model,
                &tag_model,
                &internal,
                &actions_model,
                &noname_model,
                &groups_model,
                &page_model,
            ],
        )
        .await
        .unwrap();
    Seeded {
        demo,
        runner,
        admin,
        group_runner,
        partner_model,
        country_model,
        tag_model,
        noname_model,
        page_model,
        parent_field,
        children_field,
        tags_field,
        city_field,
    }
}

async fn policy_orm_on(h: &Harness, pool: erp_testkit::deadpool_postgres::Pool) -> Orm {
    let source = PolicySource::build(Arc::clone(h.registry()), support::pool(h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(h.registry()), pool)
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();
    // this ORM is its own lineage: its audit store state is checked like a server's at startup
    assert!(orm.check_audit_store().await.unwrap().is_ready());
    orm
}

async fn policy_orm(h: &Harness) -> Orm {
    policy_orm_on(h, pool(h, 2)).await
}

// ----- helpers -------------------------------------------------------------------------------------

fn s(text: &str) -> Cell {
    Cell::Str(Arc::from(text))
}

fn vals(pairs: &[(&'static str, Cell)]) -> RawVals {
    pairs
        .iter()
        .fold(RawVals::new(), |v, (k, c)| v.set_cell(*k, c.clone()))
}

fn rid(id: i64) -> RecId {
    RecId::from_real(id).unwrap()
}

fn real(ids: &[i64]) -> Vec<RealId> {
    ids.iter().map(|id| RealId::new(*id).unwrap()).collect()
}

async fn create(tx: &mut Transaction, model: &str, v: RawVals) -> i64 {
    let records = tx.env().records(model).unwrap();
    records.create(tx, vec![v]).await.unwrap().ids()[0]
}

/// A server action on `model_id` with `pairs`, created as the superuser in its own transaction.
async fn action(h: &Harness, name: &str, model_id: i64, state: &str, extra: RawVals) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let mut v = extra
        .set_cell("name", s(name))
        .set_cell("model_id", Cell::Id(model_id))
        .set_cell("state", s(state));
    if v.get("sequence").is_none() {
        v = v.set_cell("sequence", Cell::Int(5));
    }
    let id = create(&mut tx, "ir.actions.server", v).await;
    tx.commit().await.unwrap();
    id
}

/// A record created as the superuser.
async fn record(h: &Harness, model: &str, v: RawVals) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let id = create(&mut tx, model, v).await;
    tx.commit().await.unwrap();
    id
}

/// Run `action` as `uid`; committed on success, rolled back otherwise.
async fn run(
    orm: &Orm,
    uid: i64,
    action: i64,
    model: &str,
    ids: &[i64],
) -> erp_fields::Result<Option<ActionResult>> {
    // an operation identity: a flagged action's audit row needs one at commit
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::new(UserId::new(uid).unwrap())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap();
    let env = tx.env();
    let ids = real(ids);
    let request = RunRequest::new(ActionId::new(action).unwrap(), model, &ids);
    let result = run_server_action(&mut tx, &env, &request).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    } else {
        tx.rollback().await.unwrap();
    }
    result
}

/// The typed error behind `error`.
fn typed(error: &Error) -> &ServerActionError {
    std::error::Error::source(error)
        .and_then(|s| s.downcast_ref::<ServerActionError>())
        .unwrap_or_else(|| panic!("no ServerActionError behind {error:?}"))
}

/// `error` has `kind`, `message` and the typed source `expected`.
fn assert_typed(error: &Error, kind: ErrorKind, message: &str, expected: &ServerActionError) {
    assert_eq!(error.kind(), kind, "{error:?}");
    assert_eq!(error.message(), message);
    assert_eq!(typed(error), expected);
}

/// A field of a record, read as the superuser.
async fn read(h: &Harness, model: &str, id: i64, field: &str) -> Value {
    let mut tx = h.begin().await.unwrap();
    let value = tx
        .env()
        .records(model)
        .unwrap()
        .browse([id])
        .unwrap()
        .value(&mut tx, field)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    value
}

/// Ids of `model` named `name`, as the superuser.
async fn named(client: &Client, table: &str, name: &str) -> Vec<i64> {
    client
        .query(
            &format!("SELECT id::int8 FROM {table} WHERE name = $1 ORDER BY id"),
            &[&name],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

const ACCESS: &str = "You don't have enough access rights to run this action.";

// ----- tests ---------------------------------------------------------------------------------------

/// `test_40_multi`: a `multi` action whose children are a real Luau
/// act_window code body, `object_create`, `object_write` of the city and a real Luau
/// act_url code body creates exactly one
/// record, writes the city and returns the act_url dict, all as the caller; setting the action as
/// its own child is a `ValidationError`.
#[tokio::test]
async fn access_multi_loop() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;

    // test_55: no write access on countries
    let on_country = action(
        &h,
        "TestAction",
        seeded.country_model,
        "code",
        vals(&[("code", s("env.write('x.country', {}, {name='X'})"))]),
    )
    .await;
    let error = run(&orm, seeded.demo, on_country, "x.country", &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), ACCESS);
    assert!(matches!(
        typed(&error),
        ServerActionError::AccessDenied { detail: Some(_) }
    ));

    // test_40: the multi action
    let country = record(&h, "x.country", vals(&[("name", s("TestingCountry"))])).await;
    let partner = record(
        &h,
        "x.partner",
        vals(&[
            ("name", s("TestingPartner")),
            ("city", s("OrigCity")),
            ("country_id", Cell::Id(country)),
        ]),
    )
    .await;
    // created in another order than their sequence, so the run order is the sequence's
    let url = action(
        &h,
        "Subaction3",
        seeded.partner_model,
        "code",
        vals(&[
            ("sequence", Cell::Int(4)),
            (
                "code",
                s("action={type='ir.actions.act_url',url='/record/' .. record.id}"),
            ),
        ]),
    )
    .await;
    let write_city = action(
        &h,
        "Subaction2",
        seeded.partner_model,
        "object_write",
        vals(&[
            ("sequence", Cell::Int(3)),
            ("update_path", s("city")),
            ("value", s("RaoulettePoiluchette")),
        ]),
    )
    .await;
    let create_partner = action(
        &h,
        "Subaction2",
        seeded.partner_model,
        "object_create",
        vals(&[
            ("sequence", Cell::Int(2)),
            ("crud_model_id", Cell::Id(seeded.partner_model)),
            ("value", s("RaoulettePoiluchette")),
        ]),
    )
    .await;
    let window = action(
        &h,
        "Subaction1",
        seeded.partner_model,
        "code",
        vals(&[
            ("sequence", Cell::Int(1)),
            (
                "code",
                s("return {type='ir.actions.act_window', res_model='x.partner'}"),
            ),
        ]),
    )
    .await;
    let multi = action(
        &h,
        "TestAction",
        seeded.partner_model,
        "multi",
        RawVals::new().set_commands(
            "child_ids",
            [Command::set([
                rid(url),
                rid(write_city),
                rid(create_partner),
                rid(window),
            ])],
        ),
    )
    .await;
    let result = run(&orm, seeded.demo, multi, "x.partner", &[partner])
        .await
        .unwrap()
        .expect("the act_url dict");
    assert_eq!(
        result.into_value(),
        json!({"type": "ir.actions.act_url", "url": format!("/record/{partner}")})
    );
    let created = named(&client, "x_partner", "RaoulettePoiluchette").await;
    assert_eq!(created.len(), 1, "exactly one record created");
    assert_eq!(
        read(&h, "x.partner", partner, "city").await,
        Value::str("RaoulettePoiluchette")
    );
    // the record was created by the caller (as the caller, not as the superuser)
    let creator: i64 = client
        .query_one(
            "SELECT create_uid::int8 FROM x_partner WHERE id = $1",
            &[&created[0]],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(creator, seeded.demo);

    // the order is the children's (sequence, name, id): the last result wins only when not
    // empty, so moving the url child first returns the window's dict
    let mut tx = h.begin().await.unwrap();
    tx.env()
        .records("ir.actions.server")
        .unwrap()
        .browse([url])
        .unwrap()
        .write(&mut tx, vals(&[("sequence", Cell::Int(0))]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let result = run(&orm, seeded.demo, multi, "x.partner", &[partner])
        .await
        .unwrap()
        .expect("the act_window dict");
    assert_eq!(result.action_type(), "ir.actions.act_window");

    // test_80: a permitted run as the demo user
    let rename = action(
        &h,
        "Rename",
        seeded.partner_model,
        "code",
        vals(&[(
            "code",
            s("env.write('x.partner', {record.id}, {name='2026-10-22'})"),
        )]),
    )
    .await;
    assert_eq!(
        run(&orm, seeded.demo, rename, "x.partner", &[partner])
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        read(&h, "x.partner", partner, "name").await,
        Value::str("2026-10-22")
    );

    // loops: the action as its own child, a 2-cycle, a deep chain closing on its start
    let loop_error = |e: erp_fields::Result<()>| {
        let e = e.unwrap_err();
        assert_typed(
            &e,
            ErrorKind::Validation,
            "Recursion found in child server actions",
            &ServerActionError::ChildLoop,
        );
    };
    let mut tx = h.begin().await.unwrap();
    let servers = tx.env().records("ir.actions.server").unwrap();
    loop_error(
        servers
            .browse([multi])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands("child_ids", [Command::set([rid(multi)])]),
            )
            .await,
    );
    // a rolled-back transaction keeps its pooled connection until dropped (review R-5)
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    // window is a child of multi: making multi a child of window closes a 2-cycle
    loop_error(
        servers
            .browse([multi])
            .unwrap()
            .write(&mut tx, vals(&[("parent_id", Cell::Id(window))]))
            .await,
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // a chain multi <- a <- b, then multi's parent set to b
    let mut tx = h.begin().await.unwrap();
    let a = create(
        &mut tx,
        "ir.actions.server",
        vals(&[
            ("name", s("a")),
            ("model_id", Cell::Id(seeded.partner_model)),
            ("state", s("multi")),
            ("parent_id", Cell::Id(multi)),
        ]),
    )
    .await;
    let b = create(
        &mut tx,
        "ir.actions.server",
        vals(&[
            ("name", s("b")),
            ("model_id", Cell::Id(seeded.partner_model)),
            ("state", s("multi")),
            ("parent_id", Cell::Id(a)),
        ]),
    )
    .await;
    loop_error(
        servers
            .browse([multi])
            .unwrap()
            .write(&mut tx, vals(&[("parent_id", Cell::Id(b))]))
            .await,
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // a child the caller may not write fails with the engine's refusal
    let rename_country = action(
        &h,
        "Rename country",
        seeded.partner_model,
        "object_write",
        vals(&[
            ("update_path", s("country_id.name")),
            ("value", s("Elsewhere")),
        ]),
    )
    .await;
    let elevated = action(
        &h,
        "Elevated",
        seeded.partner_model,
        "multi",
        RawVals::new().set_commands("child_ids", [Command::set([rid(rename_country)])]),
    )
    .await;
    // what the engine says when the caller writes the country directly
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let direct = tx
        .env()
        .records("x.country")
        .unwrap()
        .browse([country])
        .unwrap()
        .write(&mut tx, vals(&[("name", s("Elsewhere"))]))
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    drop(tx);
    let error = run(&orm, seeded.demo, elevated, "x.partner", &[partner])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), direct.message());
    assert_eq!(
        read(&h, "x.country", country, "name").await,
        Value::str("TestingCountry")
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn groups_replace_the_write_check() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let country = record(&h, "x.country", vals(&[("name", s("C"))])).await;
    let grouped = action(
        &h,
        "Grouped",
        seeded.country_model,
        "object_write",
        vals(&[("update_path", s("code")), ("value", s("CC"))])
            .set_commands("group_ids", [Command::link(rid(seeded.group_runner))]),
    )
    .await;
    let error = run(&orm, seeded.demo, grouped, "x.country", &[country])
        .await
        .unwrap_err();
    assert_typed(
        &error,
        ErrorKind::Access,
        ACCESS,
        &ServerActionError::AccessDenied { detail: None },
    );
    // the runner of the group passes the check; its write is still its own (as the caller), and
    // it may not write countries
    let error = run(&orm, seeded.runner, grouped, "x.country", &[country])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_ne!(
        error.message(),
        ACCESS,
        "refused by the write, not the check"
    );

    // a grouped action on partners runs for the group's user
    let partner = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    let grouped_partner = action(
        &h,
        "Grouped partner",
        seeded.partner_model,
        "object_write",
        vals(&[("update_path", s("city")), ("value", s("Bonn"))])
            .set_commands("group_ids", [Command::link(rid(seeded.group_runner))]),
    )
    .await;
    run(
        &orm,
        seeded.runner,
        grouped_partner,
        "x.partner",
        &[partner],
    )
    .await
    .unwrap();
    assert_eq!(
        read(&h, "x.partner", partner, "city").await,
        Value::str("Bonn")
    );

    // record rules: no write on a partner named Locked, so no run
    let locked = record(&h, "x.partner", vals(&[("name", s("Locked"))])).await;
    let open = action(
        &h,
        "Open",
        seeded.partner_model,
        "object_write",
        vals(&[("update_path", s("city")), ("value", s("Bonn"))]),
    )
    .await;
    let error = run(&orm, seeded.demo, open, "x.partner", &[locked])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), ACCESS);
    assert!(matches!(
        typed(&error),
        ServerActionError::AccessDenied { detail: Some(_) }
    ));
    assert_eq!(read(&h, "x.partner", locked, "city").await, Value::False);
    h.cleanup().await.unwrap();
}

/// Warnings refuse the run: a child on another model, a
/// child whose groups differ from the action's, a child with warnings of its own, a JSON field
/// at the end of `update_path`, a sequence on a field that is not text. The message names the
/// action run.
#[tokio::test]
async fn warnings_refuse_the_run() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    let warned = |name: &str| ServerActionError::HasWarnings {
        name: name.to_owned(),
    };
    let message =
        |name: &str| format!("Server action {name} has one or more warnings, address them first.");
    let check = async |id: i64, name: &str| {
        let error = run(&orm, seeded.admin, id, "x.partner", &[partner])
            .await
            .unwrap_err();
        assert_typed(&error, ErrorKind::UserError, &message(name), &warned(name));
    };

    let on_country = action(
        &h,
        "On country",
        seeded.country_model,
        "x_open_window",
        RawVals::new(),
    )
    .await;
    let other_model = action(
        &h,
        "Other model",
        seeded.partner_model,
        "multi",
        RawVals::new().set_commands("child_ids", [Command::set([rid(on_country)])]),
    )
    .await;
    check(other_model, "Other model").await;

    let ungrouped = action(
        &h,
        "Ungrouped",
        seeded.partner_model,
        "x_open_window",
        RawVals::new(),
    )
    .await;
    let other_groups = action(
        &h,
        "Other groups",
        seeded.partner_model,
        "multi",
        RawVals::new()
            .set_commands("child_ids", [Command::set([rid(ungrouped)])])
            .set_commands("group_ids", [Command::link(rid(seeded.group_runner))]),
    )
    .await;
    // the admin is not in the group: give it the group so the access check passes
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&seeded.admin, &seeded.group_runner],
        )
        .await
        .unwrap();
    check(other_groups, "Other groups").await;

    let json_write = action(
        &h,
        "Json",
        seeded.partner_model,
        "object_write",
        vals(&[("update_path", s("data")), ("value", s("{}"))]),
    )
    .await;
    check(json_write, "Json").await;
    let parent_of_warned = action(
        &h,
        "Parent",
        seeded.partner_model,
        "multi",
        RawVals::new().set_commands("child_ids", [Command::set([rid(json_write)])]),
    )
    .await;
    check(parent_of_warned, "Parent").await;

    let sequence_on_int = action(
        &h,
        "Sequence",
        seeded.partner_model,
        "object_write",
        vals(&[
            ("update_path", s("count")),
            ("evaluation_type", s("sequence")),
        ]),
    )
    .await;
    check(sequence_on_int, "Sequence").await;
    h.cleanup().await.unwrap();
}

/// Types without a runner yet name their task; an unknown state (a row written outside the ORM)
/// and a state with no runner are errors, never a silent no-op; an action that does not exist,
/// an action of another model than the active records', a runner returning something that is not
/// an action, are refused.
#[tokio::test]
async fn unsupported_and_unknown_types() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    for (state, task) in [
        ("object_copy", "a later release"),
        ("webhook", "a later release"),
    ] {
        let id = action(&h, state, seeded.partner_model, state, RawVals::new()).await;
        // also without active records: these types are declared multi until they run
        for ids in [&[partner][..], &[]] {
            let error = run(&orm, seeded.demo, id, "x.partner", ids)
                .await
                .unwrap_err();
            assert_typed(
                &error,
                ErrorKind::UserError,
                &format!("Server actions of type '{state}' is not supported until {task}."),
                &ServerActionError::Unsupported {
                    feature: format!("Server actions of type '{state}'"),
                    task,
                },
            );
        }
    }
    for (kind, label) in [(
        "sequence",
        "Update Record from a sequence (evaluation type 'sequence')",
    )] {
        let id = action(
            &h,
            kind,
            seeded.partner_model,
            "object_write",
            vals(&[("update_path", s("city")), ("evaluation_type", s(kind))]),
        )
        .await;
        let error = run(&orm, seeded.demo, id, "x.partner", &[partner])
            .await
            .unwrap_err();
        assert_typed(
            &error,
            ErrorKind::UserError,
            &format!("{label} is not supported until secured base sequence dispatch."),
            &ServerActionError::Unsupported {
                feature: label.to_owned(),
                task: "secured base sequence dispatch",
            },
        );
    }

    let gone = action(
        &h,
        "Gone",
        seeded.partner_model,
        "x_open_window",
        RawVals::new(),
    )
    .await;
    client
        .execute(
            "UPDATE ir_act_server SET state = 'x_gone' WHERE id = $1",
            &[&gone],
        )
        .await
        .unwrap();
    let error = run(&orm, seeded.demo, gone, "x.partner", &[partner])
        .await
        .unwrap_err();
    assert_typed(
        &error,
        ErrorKind::Programming,
        "Server action Gone has the type 'x_gone', which ir.actions.server.state does not have \
         in this registry.",
        &ServerActionError::UnknownState {
            name: "Gone".to_owned(),
            state: "x_gone".to_owned(),
        },
    );

    let norunner = action(
        &h,
        "No runner",
        seeded.partner_model,
        "x_norunner",
        RawVals::new(),
    )
    .await;
    let error = run(&orm, seeded.demo, norunner, "x.partner", &[partner])
        .await
        .unwrap_err();
    assert_typed(
        &error,
        ErrorKind::Programming,
        "No runner for server actions of type 'x_norunner': the addon that adds the type must \
         declare one with server_action_runner!.",
        &ServerActionError::MissingRunner {
            state: "x_norunner".to_owned(),
        },
    );

    // the action of an id no server action has (a window action's, from the shared sequence)
    let window = record(
        &h,
        "ir.actions.act_window",
        vals(&[("name", s("W")), ("res_model", s("x.partner"))]),
    )
    .await;
    let error = run(&orm, seeded.demo, window, "x.partner", &[partner])
        .await
        .unwrap_err();
    assert_typed(
        &error,
        ErrorKind::Missing,
        &format!("Server action {window} does not exist."),
        &ServerActionError::NotFound {
            id: ActionId::new(window).unwrap(),
        },
    );

    let error = run(&orm, seeded.demo, norunner, "x.country", &[])
        .await
        .unwrap_err();
    assert_typed(
        &error,
        ErrorKind::Validation,
        "Server action No runner runs on model 'x.partner', not on 'x.country'.",
        &ServerActionError::ModelMismatch {
            name: "No runner".to_owned(),
            action_model: "x.partner".to_owned(),
            active_model: "x.country".to_owned(),
        },
    );

    // the result type is validated where it is built
    assert_eq!(
        ActionResult::new(json!({"url": "/"})).unwrap_err(),
        ServerActionError::InvalidResult {
            reason: "it has no 'type'".to_owned()
        }
    );
    for (value, reason) in [
        (json!([]), "it is not a JSON object"),
        (json!({"type": ""}), "its 'type' is empty"),
        (json!({"type": 3}), "its 'type' is not a string"),
    ] {
        let error = Error::from(ActionResult::new(value).unwrap_err());
        assert_typed(
            &error,
            ErrorKind::Programming,
            &format!("A server action runner returned a result that is not an action: {reason}."),
            &ServerActionError::InvalidResult {
                reason: reason.to_owned(),
            },
        );
    }
    h.cleanup().await.unwrap();
}

/// A nested `multi` chain of exactly [`MAX_DEPTH`] levels runs; one more level is refused before
/// anything runs. A tree of exactly [`MAX_ACTIONS`] actions runs; one more is refused.
#[tokio::test]
async fn depth_and_size_limits() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    // a chain: top multi -> ... -> leaf act_window, `levels` actions deep
    let chain = async |levels: usize| -> i64 {
        let mut tx = h.begin().await.unwrap();
        let mut child = create(
            &mut tx,
            "ir.actions.server",
            vals(&[
                ("name", s("leaf")),
                ("model_id", Cell::Id(seeded.partner_model)),
                ("state", s("x_open_window")),
            ]),
        )
        .await;
        for level in 1..levels {
            child = create(
                &mut tx,
                "ir.actions.server",
                vals(&[
                    ("name", s(&format!("level {level}"))),
                    ("model_id", Cell::Id(seeded.partner_model)),
                    ("state", s("multi")),
                ])
                .set_commands("child_ids", [Command::link(rid(child))]),
            )
            .await;
        }
        tx.commit().await.unwrap();
        child
    };
    let deep = chain(MAX_DEPTH).await;
    let result = run(&orm, seeded.demo, deep, "x.partner", &[partner])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.action_type(), "ir.actions.act_window");
    let too_deep = chain(MAX_DEPTH + 1).await;
    let error = run(&orm, seeded.demo, too_deep, "x.partner", &[partner])
        .await
        .unwrap_err();
    assert_typed(
        &error,
        ErrorKind::UserError,
        "The server action has more than 8 levels of child actions; the run is refused.",
        &ServerActionError::Limit {
            kind: RunLimit::Depth,
        },
    );

    // wide: a multi with `children` children
    let wide = async |children: usize| -> i64 {
        let mut tx = h.begin().await.unwrap();
        let top = create(
            &mut tx,
            "ir.actions.server",
            vals(&[
                ("name", s("top")),
                ("model_id", Cell::Id(seeded.partner_model)),
                ("state", s("multi")),
            ]),
        )
        .await;
        let list = (0..children)
            .map(|i| {
                vals(&[
                    ("name", s(&format!("child {i:03}"))),
                    ("model_id", Cell::Id(seeded.partner_model)),
                    ("state", s("x_open_window")),
                    ("parent_id", Cell::Id(top)),
                ])
            })
            .collect();
        tx.env()
            .records("ir.actions.server")
            .unwrap()
            .create(&mut tx, list)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        top
    };
    let full = wide(MAX_ACTIONS - 1).await;
    assert!(
        run(&orm, seeded.demo, full, "x.partner", &[partner])
            .await
            .unwrap()
            .is_some()
    );
    let over = wide(MAX_ACTIONS).await;
    let error = run(&orm, seeded.demo, over, "x.partner", &[partner])
        .await
        .unwrap_err();
    assert_typed(
        &error,
        ErrorKind::UserError,
        "The server action has more than 256 actions in one run; the run is refused.",
        &ServerActionError::Limit {
            kind: RunLimit::Actions,
        },
    );

    // a loop written outside the ORM is stopped by the bounds, not followed forever
    client
        .execute(
            "UPDATE ir_act_server SET parent_id = $1 WHERE id = $2",
            &[&deep, &deep],
        )
        .await
        .unwrap();
    let error = run(&orm, seeded.demo, deep, "x.partner", &[partner])
        .await
        .unwrap_err();
    assert_typed(
        &error,
        ErrorKind::UserError,
        "The server action has more than 8 levels of child actions; the run is refused.",
        &ServerActionError::Limit {
            kind: RunLimit::Depth,
        },
    );
    h.cleanup().await.unwrap();
}

/// `run_as_superuser` (design m2-s3 §7.2): only an administrator sets it (a non-admin is refused
/// for a value and for a context default alike) and only on an action with groups (also when the
/// groups are emptied later); a non-admin may not change a flagged action at all; superuser mode
/// (the loader) counts as an administrator. `init` removes the check from an older
/// database.
#[tokio::test]
async fn run_as_superuser_rules() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let base = || {
        vals(&[
            ("name", s("Flag")),
            ("model_id", Cell::Id(seeded.partner_model)),
            ("state", s("x_open_window")),
        ])
    };
    let with_group = || base().set_commands("group_ids", [Command::link(rid(seeded.group_runner))]);
    let admin_only = |error: Error| {
        assert_typed(
            &error,
            ErrorKind::Access,
            "Only administrators (Settings) can set Run as Superuser on a server action or change \
             an action that has it.",
            &ServerActionError::SuperuserAdminOnly,
        );
    };
    let needs_groups = |error: Error| {
        assert_typed(
            &error,
            ErrorKind::Validation,
            "Server action Flag runs as superuser: restrict it to at least one group.",
            &ServerActionError::SuperuserNeedsGroups {
                name: "Flag".to_owned(),
            },
        );
    };
    let as_user = async |uid: i64| orm.begin(UserId::new(uid).unwrap()).await.unwrap();
    let servers = |tx: &Transaction| tx.env().records("ir.actions.server").unwrap();

    // a non-admin: refused for a value and for a context default
    let mut tx = as_user(seeded.demo).await;
    admin_only(
        servers(&tx)
            .create(
                &mut tx,
                vec![with_group().set_cell("run_as_superuser", Cell::Bool(true))],
            )
            .await
            .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = as_user(seeded.demo).await;
    admin_only(
        servers(&tx)
            .with_context([("default_run_as_superuser", Value::Bool(true))])
            .create(&mut tx, vec![with_group()])
            .await
            .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // an admin: groups required, at create and when they are emptied later
    let mut tx = as_user(seeded.admin).await;
    needs_groups(
        servers(&tx)
            .create(
                &mut tx,
                vec![base().set_cell("run_as_superuser", Cell::Bool(true))],
            )
            .await
            .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = as_user(seeded.admin).await;
    let flagged = servers(&tx)
        .create(
            &mut tx,
            vec![with_group().set_cell("run_as_superuser", Cell::Bool(true))],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        read(&h, "ir.actions.server", flagged, "run_as_superuser").await,
        Value::Bool(true)
    );
    let mut tx = as_user(seeded.admin).await;
    needs_groups(
        servers(&tx)
            .browse([flagged])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_commands("group_ids", [Command::clear()]),
            )
            .await
            .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // a non-admin may not change a flagged action, nor set the flag on an existing one; an
    // unflagged action stays theirs to change
    let mut tx = as_user(seeded.demo).await;
    admin_only(
        servers(&tx)
            .browse([flagged])
            .unwrap()
            .write(&mut tx, vals(&[("name", s("Changed"))]))
            .await
            .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = as_user(seeded.demo).await;
    let plain = servers(&tx)
        .create(&mut tx, vec![with_group()])
        .await
        .unwrap()
        .ids()[0];
    servers(&tx)
        .browse([plain])
        .unwrap()
        .write(&mut tx, vals(&[("name", s("Renamed"))]))
        .await
        .unwrap();
    admin_only(
        servers(&tx)
            .browse([plain])
            .unwrap()
            .write(&mut tx, vals(&[("run_as_superuser", Cell::Bool(true))]))
            .await
            .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // superuser mode (the loader's) counts as an administrator
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        "ir.actions.server",
        with_group().set_cell("run_as_superuser", Cell::Bool(true)),
    )
    .await;
    tx.rollback().await.unwrap();
    drop(tx);

    // an database has the old check: `init` drops it, and only it
    client
        .execute(
            "ALTER TABLE ir_act_server ADD CONSTRAINT ir_act_server_run_as_superuser_not_yet \
             CHECK (run_as_superuser IS NOT TRUE) NOT VALID",
            &[],
        )
        .await
        .unwrap();
    let count = async || -> i64 {
        one(
            &client,
            "SELECT count(*) FROM pg_constraint WHERE conrelid = 'ir_act_server'::regclass \
             AND conname = 'ir_act_server_run_as_superuser_not_yet'",
            &[],
        )
        .await
    };
    assert_eq!(count().await, 1);
    let mut tx = h.begin().await.unwrap();
    erp_orm::model_hooks::call_init(&mut tx, "ir.actions.server")
        .await
        .unwrap();
    // and again with nothing to drop
    erp_orm::model_hooks::call_init(&mut tx, "ir.actions.server")
        .await
        .unwrap();
    create(
        &mut tx,
        "ir.actions.server",
        with_group().set_cell("run_as_superuser", Cell::Bool(true)),
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(count().await, 0);
    h.cleanup().await.unwrap();
}

/// `run_as_superuser` covers the flagged action's whole tree (review SEC-1): a non-admin may not
/// change a child or grandchild of a flagged action, add a child to it (create or write with
/// `parent_id` in its tree), take a child out of it through `child_ids`, delete a child, or
/// delete an unflagged parent whose cascade deletes a flagged action. Actions outside every
/// flagged tree stay theirs to change and delete; an administrator changes the tree.
#[tokio::test]
async fn flag_covers_the_tree() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let grouped =
        |extra: RawVals| extra.set_commands("group_ids", [Command::link(rid(seeded.group_runner))]);
    let leaf = async |name: &str| -> i64 {
        action(
            &h,
            name,
            seeded.partner_model,
            "x_open_window",
            grouped(RawVals::new()),
        )
        .await
    };
    let child = leaf("Child").await;
    let grandchild = leaf("Grandchild").await;
    let middle = action(
        &h,
        "Middle",
        seeded.partner_model,
        "multi",
        grouped(RawVals::new().set_commands("child_ids", [Command::set([rid(grandchild)])])),
    )
    .await;
    let flagged = action(
        &h,
        "Flagged",
        seeded.partner_model,
        "multi",
        grouped(
            vals(&[("run_as_superuser", Cell::Bool(true))])
                .set_commands("child_ids", [Command::set([rid(child), rid(middle)])]),
        ),
    )
    .await;
    // an unflagged parent of a flagged action
    let flagged_leaf = action(
        &h,
        "Flagged leaf",
        seeded.partner_model,
        "x_open_window",
        grouped(vals(&[("run_as_superuser", Cell::Bool(true))])),
    )
    .await;
    let holder = action(
        &h,
        "Holder",
        seeded.partner_model,
        "multi",
        RawVals::new().set_commands("child_ids", [Command::set([rid(flagged_leaf)])]),
    )
    .await;
    let free = leaf("Free").await;

    let admin_only = |result: erp_fields::Result<()>| {
        assert_typed(
            &result.unwrap_err(),
            ErrorKind::Access,
            "Only administrators (Settings) can set Run as Superuser on a server action or change \
             an action that has it.",
            &ServerActionError::SuperuserAdminOnly,
        );
    };
    let servers = |tx: &Transaction| tx.env().records("ir.actions.server").unwrap();
    let new_action = |pairs: RawVals| {
        pairs
            .set_cell("name", s("New"))
            .set_cell("model_id", Cell::Id(seeded.partner_model))
            .set_cell("state", s("x_open_window"))
    };
    // every case in its own transaction, rolled back and dropped (its connection goes back)
    macro_rules! as_demo {
        (|$tx:ident| $body:expr) => {{
            let mut $tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
            let result: erp_fields::Result<()> = $body;
            $tx.rollback().await.unwrap();
            drop($tx);
            result
        }};
    }

    // a child, a grandchild, a nested multi
    for id in [child, grandchild, middle] {
        admin_only(as_demo!(|tx| servers(&tx)
            .browse([id])
            .unwrap()
            .write(&mut tx, vals(&[("name", s("Changed"))]))
            .await));
    }
    // a new action under the flagged action or under a descendant of it
    for parent in [flagged, middle, grandchild] {
        admin_only(as_demo!(|tx| servers(&tx)
            .create(
                &mut tx,
                vec![new_action(vals(&[("parent_id", Cell::Id(parent))]))]
            )
            .await
            .map(|_| ())));
    }
    // an existing action moved into the tree, or a child taken out of it
    admin_only(as_demo!(|tx| servers(&tx)
        .browse([free])
        .unwrap()
        .write(&mut tx, vals(&[("parent_id", Cell::Id(middle))]))
        .await));
    admin_only(as_demo!(|tx| servers(&tx)
        .browse([free])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("child_ids", [Command::link(rid(child))]),
        )
        .await));
    admin_only(as_demo!(|tx| servers(&tx)
        .create(
            &mut tx,
            vec![new_action(
                RawVals::new().set_commands("child_ids", [Command::link(rid(grandchild))]),
            )],
        )
        .await
        .map(|_| ())));
    // deleting a child, or a parent whose cascade deletes a flagged action
    for id in [child, grandchild, holder] {
        admin_only(as_demo!(|tx| servers(&tx)
            .browse([id])
            .unwrap()
            .unlink(&mut tx)
            .await));
    }
    // outside every flagged tree: a non-admin changes and deletes
    as_demo!(|tx| {
        servers(&tx)
            .browse([free])
            .unwrap()
            .write(&mut tx, vals(&[("name", s("Renamed"))]))
            .await
            .unwrap();
        servers(&tx).browse([free]).unwrap().unlink(&mut tx).await
    })
    .unwrap();
    // an administrator changes the tree
    let mut tx = orm.begin(UserId::new(seeded.admin).unwrap()).await.unwrap();
    servers(&tx)
        .browse([grandchild])
        .unwrap()
        .write(&mut tx, vals(&[("name", s("Changed by admin"))]))
        .await
        .unwrap();
    servers(&tx)
        .browse([child])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        read(&h, "ir.actions.server", grandchild, "name").await,
        Value::str("Changed by admin")
    );
    h.cleanup().await.unwrap();
}

/// The flag check locks what it read (review SEC-3): while an administrator's uncommitted
/// transaction flags a parent, a non-admin's write on its child waits for it instead of slipping
/// in on its snapshot (here it gives up at its lock timeout).
#[tokio::test]
async fn flag_check_locks_the_tree() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let child = action(
        &h,
        "Child",
        seeded.partner_model,
        "x_open_window",
        RawVals::new(),
    )
    .await;
    let parent = action(
        &h,
        "Parent",
        seeded.partner_model,
        "multi",
        RawVals::new()
            .set_commands("child_ids", [Command::set([rid(child)])])
            .set_commands("group_ids", [Command::link(rid(seeded.group_runner))]),
    )
    .await;
    // the administrator's transaction (superuser mode, the loader's) flags the parent
    let mut admin = h.begin().await.unwrap();
    admin
        .env()
        .records("ir.actions.server")
        .unwrap()
        .browse([parent])
        .unwrap()
        .write(&mut admin, vals(&[("run_as_superuser", Cell::Bool(true))]))
        .await
        .unwrap();
    admin.flush().await.unwrap();
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    tx.batch("SET LOCAL lock_timeout = '300ms'").await.unwrap();
    let error = tx
        .env()
        .records("ir.actions.server")
        .unwrap()
        .browse([child])
        .unwrap()
        .write(&mut tx, vals(&[("name", s("Changed"))]))
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), Some("55P03"), "{error:?}");
    tx.rollback().await.unwrap();
    drop(tx);
    admin.rollback().await.unwrap();
    drop(admin);
    assert_eq!(
        read(&h, "ir.actions.server", child, "name").await,
        Value::str("Child")
    );
    h.cleanup().await.unwrap();
}

/// A flagged action that lost its groups outside the write rules (a group deleted, a cascade,
/// SQL) is refused at run time, also as a child (review SEC-2): the run never falls back to the
/// write check for it.
#[tokio::test]
async fn groupless_flagged_action_does_not_run() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    let flagged = action(
        &h,
        "Flag",
        seeded.partner_model,
        "object_write",
        vals(&[
            ("update_path", s("city")),
            ("value", s("Bonn")),
            ("run_as_superuser", Cell::Bool(true)),
        ])
        .set_commands("group_ids", [Command::link(rid(seeded.group_runner))]),
    )
    .await;
    let parent = action(
        &h,
        "Parent",
        seeded.partner_model,
        "multi",
        RawVals::new().set_commands("child_ids", [Command::set([rid(flagged)])]),
    )
    .await;
    client
        .execute(
            "DELETE FROM ir_act_server_group_rel WHERE act_id = $1",
            &[&flagged],
        )
        .await
        .unwrap();
    let expected = ServerActionError::SuperuserNeedsGroups {
        name: "Flag".to_owned(),
    };
    let message = "Server action Flag runs as superuser: restrict it to at least one group.";
    for id in [flagged, parent] {
        let error = run(&orm, seeded.demo, id, "x.partner", &[partner])
            .await
            .unwrap_err();
        assert_typed(&error, ErrorKind::Validation, message, &expected);
    }
    assert_eq!(read(&h, "x.partner", partner, "city").await, Value::False);
    h.cleanup().await.unwrap();
}

/// Every action of the tree is checked before the first one runs (review R-1): a `multi` whose
/// first child writes and whose later child has no runner, an unknown type, or the superuser
/// flag without groups refuses the run with nothing written, seen inside the caller's own
/// transaction.
#[tokio::test]
async fn refusals_come_before_any_write() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let partner = record(
        &h,
        "x.partner",
        vals(&[("name", s("P")), ("city", s("Before"))]),
    )
    .await;
    let multi = async |name: &str, later: i64| -> i64 {
        let first = action(
            &h,
            "First",
            seeded.partner_model,
            "object_write",
            vals(&[
                ("sequence", Cell::Int(1)),
                ("update_path", s("city")),
                ("value", s("After")),
            ]),
        )
        .await;
        action(
            &h,
            name,
            seeded.partner_model,
            "multi",
            RawVals::new().set_commands("child_ids", [Command::set([rid(first), rid(later)])]),
        )
        .await
    };
    let later = async |name: &str, state: &str, extra: RawVals| -> i64 {
        action(
            &h,
            name,
            seeded.partner_model,
            state,
            extra.set_cell("sequence", Cell::Int(2)),
        )
        .await
    };
    let norunner = later("No runner", "x_norunner", RawVals::new()).await;
    let gone = later("Gone", "x_open_window", RawVals::new()).await;
    client
        .execute(
            "UPDATE ir_act_server SET state = 'x_gone' WHERE id = $1",
            &[&gone],
        )
        .await
        .unwrap();
    // a flag without groups cannot be written through the ORM: set it behind its back
    let flagged = later("Flagged", "x_open_window", RawVals::new()).await;
    client
        .execute(
            "UPDATE ir_act_server SET run_as_superuser = true WHERE id = $1",
            &[&flagged],
        )
        .await
        .unwrap();
    let cases: [(i64, ErrorKind, &str, ServerActionError); 3] = [
        (
            multi("With no runner", norunner).await,
            ErrorKind::Programming,
            "No runner for server actions of type 'x_norunner': the addon that adds the type \
             must declare one with server_action_runner!.",
            ServerActionError::MissingRunner {
                state: "x_norunner".to_owned(),
            },
        ),
        (
            multi("With gone", gone).await,
            ErrorKind::Programming,
            "Server action Gone has the type 'x_gone', which ir.actions.server.state does not \
             have in this registry.",
            ServerActionError::UnknownState {
                name: "Gone".to_owned(),
                state: "x_gone".to_owned(),
            },
        ),
        (
            multi("With flagged", flagged).await,
            ErrorKind::Validation,
            "Server action Flagged runs as superuser: restrict it to at least one group.",
            ServerActionError::SuperuserNeedsGroups {
                name: "Flagged".to_owned(),
            },
        ),
    ];
    for (id, kind, message, expected) in cases {
        let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
        let env = tx.env();
        let ids = real(&[partner]);
        let error = run_server_action(
            &mut tx,
            &env,
            &RunRequest::new(ActionId::new(id).unwrap(), "x.partner", &ids),
        )
        .await
        .unwrap_err();
        assert_typed(&error, kind, message, &expected);
        let city = tx
            .env()
            .records("x.partner")
            .unwrap()
            .browse([partner])
            .unwrap()
            .value(&mut tx, "city")
            .await
            .unwrap();
        assert_eq!(
            city,
            Value::str("Before"),
            "nothing written before the refusal"
        );
        tx.rollback().await.unwrap();
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// A stored selection the run needs but that is empty (NULL, written outside the ORM) is refused
/// before any action of the tree runs (review TD-D1, SF-D1): `evaluation_type` of an
/// `object_write`, and `update_m2m_operation` of an `object_write` on a many2many field. Each is
/// the second child of a `multi` whose first child is a valid `object_write`; that first write
/// is not done, seen inside the caller's own transaction and again in a fresh read.
#[tokio::test]
async fn null_selections_refused_before_any_write() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let tag = record(&h, "x.tag", vals(&[("name", s("T"))])).await;
    let partner = record(
        &h,
        "x.partner",
        vals(&[("name", s("P")), ("city", s("Before"))]),
    )
    .await;
    // `column` set to NULL behind the ORM's back
    let later = async |name: &str, extra: RawVals, column: &str| -> i64 {
        let id = action(
            &h,
            name,
            seeded.partner_model,
            "object_write",
            extra.set_cell("sequence", Cell::Int(2)),
        )
        .await;
        client
            .execute(
                &format!("UPDATE ir_act_server SET {column} = NULL WHERE id = $1"),
                &[&id],
            )
            .await
            .unwrap();
        id
    };
    let multi = async |name: &str, later: i64| -> i64 {
        let first = action(
            &h,
            "First",
            seeded.partner_model,
            "object_write",
            vals(&[
                ("sequence", Cell::Int(1)),
                ("update_path", s("city")),
                ("value", s("After")),
            ]),
        )
        .await;
        action(
            &h,
            name,
            seeded.partner_model,
            "multi",
            RawVals::new().set_commands("child_ids", [Command::set([rid(first), rid(later)])]),
        )
        .await
    };
    let no_type = later(
        "No type",
        vals(&[("update_path", s("city")), ("value", s("Later"))]),
        "evaluation_type",
    )
    .await;
    let no_op = later(
        "No operation",
        vals(&[
            ("update_path", s("tag_ids")),
            ("value", s(&tag.to_string())),
        ]),
        "update_m2m_operation",
    )
    .await;
    let no_type_reason = "its value type is empty, not one of value, sequence, equation";
    let no_op_reason = "it updates the many2many field x.partner.tag_ids, but its many2many \
                        operation is empty, not one of add, remove, set, clear";
    let cases = [
        (
            multi("With no type", no_type).await,
            "No type",
            no_type_reason,
        ),
        (
            multi("With no operation", no_op).await,
            "No operation",
            no_op_reason,
        ),
    ];
    for (id, name, reason) in cases {
        let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
        let env = tx.env();
        let ids = real(&[partner]);
        let error = run_server_action(
            &mut tx,
            &env,
            &RunRequest::new(ActionId::new(id).unwrap(), "x.partner", &ids),
        )
        .await
        .unwrap_err();
        assert_typed(
            &error,
            ErrorKind::Validation,
            &format!("Server action {name} cannot run: {reason}."),
            &ServerActionError::Misconfigured {
                name: name.to_owned(),
                reason: reason.to_owned(),
            },
        );
        let city = tx
            .env()
            .records("x.partner")
            .unwrap()
            .browse([partner])
            .unwrap()
            .value(&mut tx, "city")
            .await
            .unwrap();
        assert_eq!(
            city,
            Value::str("Before"),
            "{name}: the first child wrote nothing before the refusal"
        );
        tx.rollback().await.unwrap();
        drop(tx);
        // and in a fresh read, after the caller's transaction is gone
        assert_eq!(
            read(&h, "x.partner", partner, "city").await,
            Value::str("Before"),
            "{name}: fresh read"
        );
    }
    let tags: i64 = one(
        &client,
        "SELECT count(*)::int8 FROM x_partner_x_tag_rel WHERE x_partner_id = $1",
        &[&partner],
    )
    .await;
    assert_eq!(tags, 0, "no tag linked");
    h.cleanup().await.unwrap();
}

/// The cascade from `ir.model.fields` to `link_field_id` finds its rows by an index (review
/// DB-1), as for `update_field_id`'s sibling `model_id`.
#[tokio::test]
async fn link_field_is_indexed() {
    let h = harness().await;
    let client = connect(&h).await;
    let indexed = one(
        &client,
        "SELECT count(*) FROM pg_index i \
           JOIN pg_class c ON c.oid = i.indrelid \
           JOIN pg_namespace n ON n.oid = c.relnamespace \
           JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = i.indkey[0] \
          WHERE n.nspname = current_schema() AND c.relname = 'ir_act_server' \
            AND a.attname = 'link_field_id'",
        &[],
    )
    .await;
    assert_eq!(indexed, 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn object_write_conversions() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let parent = record(&h, "x.partner", vals(&[("name", s("Parent"))])).await;
    let partner = record(
        &h,
        "x.partner",
        vals(&[("name", s("P")), ("parent_id", Cell::Id(parent))]),
    )
    .await;
    let tag1 = record(&h, "x.tag", vals(&[("name", s("t1"))])).await;
    let tag2 = record(&h, "x.tag", vals(&[("name", s("t2"))])).await;
    let country = record(&h, "x.country", vals(&[("name", s("C"))])).await;
    let write = async |path: &str, extra: RawVals| -> erp_fields::Result<Option<ActionResult>> {
        let id = action(
            &h,
            "W",
            seeded.partner_model,
            "object_write",
            extra.set_cell("update_path", s(path)),
        )
        .await;
        run(&orm, seeded.demo, id, "x.partner", &[partner]).await
    };
    let value = |v: &str| vals(&[("value", s(v))]);
    let bad = |field: &str, v: &str, expected: &'static str| {
        (
            format!("Server action W: '{v}' is not a valid {expected} for x.partner.{field}."),
            ServerActionError::BadValue {
                name: "W".to_owned(),
                field: format!("x.partner.{field}"),
                value: v.to_owned(),
                expected,
            },
        )
    };
    let field = async |name: &str| read(&h, "x.partner", partner, name).await;

    write("count", value(" 42 ")).await.unwrap();
    assert_eq!(field("count").await, Value::Int(42));
    for (v, expected) in [
        ("abc", "integer"),
        ("1.5", "integer"),
        ("2147483648", "integer"),
    ] {
        let (message, typed_error) = bad("count", v, expected);
        let error = write("count", value(v)).await.unwrap_err();
        assert_typed(&error, ErrorKind::Validation, &message, &typed_error);
    }
    assert_eq!(
        field("count").await,
        Value::Int(42),
        "unchanged by refusals"
    );
    write("count", value("-2147483648")).await.unwrap();
    assert_eq!(field("count").await, Value::Int(i64::from(i32::MIN)));

    write("ratio", value("1.5")).await.unwrap();
    assert_eq!(field("ratio").await, Value::Float(1.5));
    let (message, typed_error) = bad("ratio", "nan", "number");
    assert_typed(
        &write("ratio", value("nan")).await.unwrap_err(),
        ErrorKind::Validation,
        &message,
        &typed_error,
    );

    write("flag", vals(&[("update_boolean_value", s("true"))]))
        .await
        .unwrap();
    assert_eq!(field("flag").await, Value::Bool(true));
    write("flag", vals(&[("update_boolean_value", s("false"))]))
        .await
        .unwrap();
    assert_eq!(field("flag").await, Value::Bool(false));

    write("day", value("2026-10-22")).await.unwrap();
    assert_eq!(
        field("day").await,
        Value::Date(erp_fields::Date::parse_odoo("2026-10-22").unwrap())
    );
    let (message, typed_error) = bad("day", "22/10/2026", "date");
    assert_typed(
        &write("day", value("22/10/2026")).await.unwrap_err(),
        ErrorKind::Validation,
        &message,
        &typed_error,
    );

    // non-ASCII text as is; a long value is quoted shortened in messages
    write("city", value("Zürich 東京")).await.unwrap();
    assert_eq!(field("city").await, Value::str("Zürich 東京"));
    let long = "9".repeat(100);
    let error = write("count", value(&long)).await.unwrap_err();
    assert_eq!(
        typed(&error),
        &ServerActionError::BadValue {
            name: "W".to_owned(),
            field: "x.partner.count".to_owned(),
            value: format!("{}…", "9".repeat(64)),
            expected: "integer",
        }
    );

    // many2one: an id, 0 clears, no value clears; a negative number is no record id (review
    // R-3: it would name a new record)
    write("country_id", value(&country.to_string()))
        .await
        .unwrap();
    assert_eq!(field("country_id").await, Value::Int(country));
    write("country_id", value("0")).await.unwrap();
    assert_eq!(field("country_id").await, Value::False);
    write("country_id", value(&country.to_string()))
        .await
        .unwrap();
    write("country_id", RawVals::new()).await.unwrap();
    assert_eq!(field("country_id").await, Value::False);
    for v in ["-5", "abc"] {
        let (message, typed_error) = bad("country_id", v, "record id");
        assert_typed(
            &write("country_id", value(v)).await.unwrap_err(),
            ErrorKind::Validation,
            &message,
            &typed_error,
        );
    }

    write("count", RawVals::new()).await.unwrap();
    assert_eq!(field("count").await, Value::Int(0));
    write("day", RawVals::new()).await.unwrap();
    assert_eq!(field("day").await, Value::False);
    write("flag", vals(&[("update_boolean_value", s("true"))]))
        .await
        .unwrap();
    let id = action(
        &h,
        "W",
        seeded.partner_model,
        "object_write",
        vals(&[("update_path", s("flag"))]),
    )
    .await;
    client
        .execute(
            "UPDATE ir_act_server SET update_boolean_value = NULL WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    run(&orm, seeded.demo, id, "x.partner", &[partner])
        .await
        .unwrap();
    assert_eq!(field("flag").await, Value::Bool(false));

    // datetime
    write("stamp", value("2026-10-22 10:30:00")).await.unwrap();
    assert_eq!(
        field("stamp").await,
        Value::DateTime(erp_fields::DateTime::parse_odoo("2026-10-22 10:30:00").unwrap())
    );
    let (message, typed_error) = bad("stamp", "yesterday", "datetime");
    assert_typed(
        &write("stamp", value("yesterday")).await.unwrap_err(),
        ErrorKind::Validation,
        &message,
        &typed_error,
    );
    // a float with digits takes a decimal
    write("amount", value("12.50")).await.unwrap();
    assert_eq!(
        field("amount").await,
        Value::Decimal(erp_fields::Decimal::from_str("12.50").unwrap())
    );
    let (message, typed_error) = bad("amount", "12,5", "number");
    assert_typed(
        &write("amount", value("12,5")).await.unwrap_err(),
        ErrorKind::Validation,
        &message,
        &typed_error,
    );

    // stored selection values outside their selection (written outside the ORM) are refused
    // before anything runs
    let stored = async |column: &str, text: &str, extra: RawVals| -> Error {
        let id = action(
            &h,
            "W",
            seeded.partner_model,
            "object_write",
            extra.set_cell("update_path", s("count")),
        )
        .await;
        client
            .execute(
                &format!("UPDATE ir_act_server SET {column} = $1 WHERE id = $2"),
                &[&text, &id],
            )
            .await
            .unwrap();
        run(&orm, seeded.demo, id, "x.partner", &[partner])
            .await
            .unwrap_err()
    };
    for (column, text, reason) in [
        (
            "evaluation_type",
            "magic",
            "its value type is 'magic', not one of value, sequence, equation",
        ),
        (
            "update_m2m_operation",
            "merge",
            "its many2many operation is 'merge', not one of add, remove, set, clear",
        ),
        (
            "update_boolean_value",
            "maybe",
            "its boolean value is 'maybe', not one of true, false",
        ),
    ] {
        assert_typed(
            &stored(column, text, value("1")).await,
            ErrorKind::Validation,
            &format!("Server action W cannot run: {reason}."),
            &ServerActionError::Misconfigured {
                name: "W".to_owned(),
                reason: reason.to_owned(),
            },
        );
    }
    assert_eq!(field("count").await, Value::Int(0), "unchanged by refusals");

    // many2many operations
    let tags = async || -> Vec<i64> {
        client
            .query(
                "SELECT x_tag_id::int8 FROM x_partner_x_tag_rel WHERE x_partner_id = $1 ORDER BY 1",
                &[&partner],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect()
    };
    let op = |o: &str, v: Option<i64>| {
        let mut x = vals(&[("update_m2m_operation", s(o))]);
        if let Some(v) = v {
            x = x.set_cell("value", s(&v.to_string()));
        }
        x
    };
    write("tag_ids", op("add", Some(tag1))).await.unwrap();
    write("tag_ids", op("add", Some(tag2))).await.unwrap();
    assert_eq!(tags().await, vec![tag1, tag2]);
    write("tag_ids", op("remove", Some(tag1))).await.unwrap();
    assert_eq!(tags().await, vec![tag2]);
    write("tag_ids", op("set", Some(tag1))).await.unwrap();
    assert_eq!(tags().await, vec![tag1]);
    write("tag_ids", op("clear", None)).await.unwrap();
    assert_eq!(tags().await, Vec::<i64>::new());
    let (message, typed_error) = bad("tag_ids", "", "record id");
    assert_typed(
        &write("tag_ids", op("add", None)).await.unwrap_err(),
        ErrorKind::Validation,
        &message,
        &typed_error,
    );

    // a dotted path writes the record at its end, as the caller
    write("parent_id.city", value("Bonn")).await.unwrap();
    assert_eq!(
        read(&h, "x.partner", parent, "city").await,
        Value::str("Bonn")
    );

    // paths that do not resolve
    let path_error = |path: &str, reason: &str| {
        (
            format!("Server action W: the path '{path}' to update {reason}."),
            ServerActionError::UpdatePath {
                name: "W".to_owned(),
                path: path.to_owned(),
                reason: reason.to_owned(),
            },
        )
    };
    for (path, reason) in [
        ("nosuch", "names 'nosuch', which x.partner does not have"),
        (
            "city.name",
            "goes through x.partner.city, a char field: only the last step may be a field that \
             is not relational",
        ),
        (
            "parent_id.nosuch",
            "names 'nosuch', which x.partner does not have",
        ),
    ] {
        let (message, typed_error) = path_error(path, reason);
        assert_typed(
            &write(path, value("x")).await.unwrap_err(),
            ErrorKind::Validation,
            &message,
            &typed_error,
        );
    }
    let deep_path = vec!["parent_id"; 16].join(".") + ".city";
    assert_typed(
        &write(&deep_path, value("x")).await.unwrap_err(),
        ErrorKind::UserError,
        "The server action has more than 16 steps in an update path; the run is refused.",
        &ServerActionError::Limit {
            kind: RunLimit::PathSteps,
        },
    );
    // the field to update must be the path's
    let (message, typed_error) = path_error(
        "name",
        "ends on x.partner.name, but its field to update is x.partner.city",
    );
    assert_typed(
        &write(
            "name",
            value("x").set_cell("update_field_id", Cell::Id(seeded.city_field)),
        )
        .await
        .unwrap_err(),
        ErrorKind::Validation,
        &message,
        &typed_error,
    );
    // no path
    assert_typed(
        &write("", value("x")).await.unwrap_err(),
        ErrorKind::Validation,
        "Server action W cannot run: it has no field to update.",
        &ServerActionError::Misconfigured {
            name: "W".to_owned(),
            reason: "it has no field to update".to_owned(),
        },
    );
    // a system column is not written, and says so (review SF-1); json is refused by the warning
    assert_typed(
        &write("id", value("1")).await.unwrap_err(),
        ErrorKind::Validation,
        "Server action W cannot update x.partner.id: it is a system column, which Update Record \
         does not write.",
        &ServerActionError::SystemField {
            name: "W".to_owned(),
            field: "x.partner.id".to_owned(),
        },
    );
    let feature = "Update Record of the html field x.partner.body (from html_value)";
    assert_typed(
        &write("body", value("<p>x</p>")).await.unwrap_err(),
        ErrorKind::UserError,
        &format!("{feature} is not supported until a later release."),
        &ServerActionError::Unsupported {
            feature: feature.to_owned(),
            task: "a later release",
        },
    );
    assert_eq!(field("body").await, Value::False);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn object_create_links_and_rec_name() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let p1 = record(&h, "x.partner", vals(&[("name", s("P1"))])).await;
    let p2 = record(&h, "x.partner", vals(&[("name", s("P2"))])).await;
    let creating = async |name: &str, extra: RawVals| -> i64 {
        action(&h, name, seeded.partner_model, "object_create", extra).await
    };

    // one per active record; the crud model defaults to the action's
    let plain = creating("Plain", vals(&[("value", s("Twice"))])).await;
    run(&orm, seeded.demo, plain, "x.partner", &[p1, p2])
        .await
        .unwrap();
    assert_eq!(named(&client, "x_partner", "Twice").await.len(), 2);
    run(&orm, seeded.demo, plain, "x.partner", &[])
        .await
        .unwrap();
    assert_eq!(named(&client, "x_partner", "Twice").await.len(), 2);

    let parent = creating(
        "Parent",
        vals(&[
            ("value", s("NewParent")),
            ("link_field_id", Cell::Id(seeded.parent_field)),
        ]),
    )
    .await;
    run(&orm, seeded.demo, parent, "x.partner", &[p1])
        .await
        .unwrap();
    let created = named(&client, "x_partner", "NewParent").await;
    assert_eq!(created.len(), 1);
    assert_eq!(
        read(&h, "x.partner", p1, "parent_id").await,
        Value::Int(created[0])
    );

    let child = creating(
        "Child",
        vals(&[
            ("value", s("NewChild")),
            ("link_field_id", Cell::Id(seeded.children_field)),
        ]),
    )
    .await;
    run(&orm, seeded.demo, child, "x.partner", &[p2])
        .await
        .unwrap();
    let created = named(&client, "x_partner", "NewChild").await;
    assert_eq!(created.len(), 1);
    assert_eq!(
        read(&h, "x.partner", created[0], "parent_id").await,
        Value::Int(p2)
    );

    let tag = creating(
        "Tag",
        vals(&[
            ("value", s("NewTag")),
            ("crud_model_id", Cell::Id(seeded.tag_model)),
            ("link_field_id", Cell::Id(seeded.tags_field)),
        ]),
    )
    .await;
    run(&orm, seeded.demo, tag, "x.partner", &[p1])
        .await
        .unwrap();
    let created = named(&client, "x_tag", "NewTag").await;
    assert_eq!(created.len(), 1);
    let linked: i64 = client
        .query_one(
            "SELECT x_tag_id::int8 FROM x_partner_x_tag_rel WHERE x_partner_id = $1",
            &[&p1],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(linked, created[0]);

    // a link field that does not point to the created model
    let wrong = creating(
        "Wrong",
        vals(&[
            ("value", s("Nope")),
            ("crud_model_id", Cell::Id(seeded.tag_model)),
            ("link_field_id", Cell::Id(seeded.parent_field)),
        ]),
    )
    .await;
    // a link field that is not relational
    let not_relational = creating(
        "Char",
        vals(&[
            ("value", s("Nope")),
            ("crud_model_id", Cell::Id(seeded.tag_model)),
            ("link_field_id", Cell::Id(seeded.city_field)),
        ]),
    )
    .await;
    // refused before the create (review R-2): nothing was created, seen inside the caller's
    // transaction before it rolls back
    for (id, name, reason) in [
        (
            wrong,
            "Wrong",
            "x.partner.parent_id does not point to x.tag",
        ),
        (
            not_relational,
            "Char",
            "x.partner.city is a char field, not a relational one",
        ),
    ] {
        let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
        let env = tx.env();
        let ids = real(&[p1]);
        let error = run_server_action(
            &mut tx,
            &env,
            &RunRequest::new(ActionId::new(id).unwrap(), "x.partner", &ids),
        )
        .await
        .unwrap_err();
        assert_typed(
            &error,
            ErrorKind::Validation,
            &format!("Server action {name}: the link field {reason}."),
            &ServerActionError::LinkField {
                name: name.to_owned(),
                reason: reason.to_owned(),
            },
        );
        tx.flush().await.unwrap();
        let found: i64 = tx
            .query("SELECT count(*) FROM x_tag WHERE name = 'Nope'", &[])
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(found, 0, "nothing created before the refusal");
        tx.rollback().await.unwrap();
        drop(tx);
    }

    let page = creating(
        "Page",
        vals(&[
            ("value", s("<p>Page</p>")),
            ("crud_model_id", Cell::Id(seeded.page_model)),
        ]),
    )
    .await;
    run(&orm, seeded.demo, page, "x.partner", &[p1])
        .await
        .unwrap();
    assert_eq!(named(&client, "x_page", "<p>Page</p>").await.len(), 1);

    let noname = creating(
        "No name",
        vals(&[
            ("value", s("x")),
            ("crud_model_id", Cell::Id(seeded.noname_model)),
        ]),
    )
    .await;
    assert_typed(
        &run(&orm, seeded.demo, noname, "x.partner", &[p1])
            .await
            .unwrap_err(),
        ErrorKind::UserError,
        "Cannot execute name_create, no _rec_name defined on x.noname",
        &ServerActionError::NoRecName {
            model: "x.noname".to_owned(),
        },
    );
    h.cleanup().await.unwrap();
}

/// Statements of one `object_write` run on one record as a user (warm: the policy loaded, the
/// user resolved and the statements prepared by a first run on another record, so the measured
/// run really writes), counted on the wire and by the transaction: pinned at the measured value
/// (ratchet). Which statements they are is not itemized here.
#[tokio::test]
async fn statements_of_one_object_write() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &PARTNER,
        &EXT_A,
        &EXT_B,
        &EXT_NORUNNER,
    ])
    .with_decls(decls())
    .count_round_trips()
    .build()
    .await
    .expect("harness");
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let wire = h.wire().expect("count_round_trips").clone();
    let orm = policy_orm_on(&h, wire.pool(2).unwrap()).await;
    let warm = record(&h, "x.partner", vals(&[("name", s("W"))])).await;
    let partner = record(&h, "x.partner", vals(&[("name", s("P"))])).await;
    let id = action(
        &h,
        "City",
        seeded.partner_model,
        "object_write",
        vals(&[("update_path", s("city")), ("value", s("Bonn"))]),
    )
    .await;
    let action_id = ActionId::new(id).unwrap();
    let warm_ids = real(&[warm]);
    let request_ids = real(&[partner]);
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    // warm: a first run on another record prepares the statements and resolves the user
    run_server_action(
        &mut tx,
        &env,
        &RunRequest::new(action_id, "x.partner", &warm_ids),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    let request = RunRequest::new(action_id, "x.partner", &request_ids);
    let before = tx.query_count();
    erp_testkit::assert_round_trips!(wire, == 3, {
        run_server_action(&mut tx, &env, &request).await.unwrap();
        tx.flush().await.unwrap();
    });
    assert_eq!(tx.query_count() - before, 3);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The runner slice's checks on a registry (`_register_hook` of `ir.actions.server`): the
/// fixture addons' registry is complete; a second runner for a state, a runner for a state the
/// selection does not have and a state without a runner are refused, named.
#[tokio::test]
async fn runner_slice_checks() {
    let registry = |extra: &[&'static AddonDescriptor]| {
        let mut addons: Vec<&'static AddonDescriptor> = vec![
            &erp_addon_base::ADDON,
            &erp_data::model_data::ADDON,
            &PARTNER,
            &EXT_A,
            &EXT_B,
        ];
        addons.extend_from_slice(extra);
        let mut d = decls();
        // the norunner extension only when its addon is there
        if !extra.iter().any(|a| a.name == "test_ext_norunner") {
            d.pop();
        }
        // the registry builder takes declarations for the life of the test binary
        let d: Vec<&'static ModelDecl> = d.into_iter().map(|x| &*Box::leak(Box::new(x))).collect();
        RegistryBuilder::new(addons).with_decls(d).build().unwrap()
    };
    // each fixture runner belongs to the descriptor it names (review TD-1)
    assert_eq!(EXT_A_RUNNER.addon(), EXT_A.name);
    assert_eq!(EXT_ORPHAN_RUNNER.addon(), EXT_ORPHAN.name);
    check_runners(&registry(&[])).unwrap();

    let error = check_runners(&registry(&[&EXT_DUP])).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    let ServerActionError::DuplicateRunner {
        state,
        first,
        second,
    } = typed(&error)
    else {
        panic!("{error:?}")
    };
    assert_eq!(state, "x_open_window");
    assert_eq!(first, &EXT_A_RUNNER.location());
    assert_eq!(second, &EXT_DUP_RUNNER.location());
    assert_eq!(
        error.message(),
        format!(
            "Server actions of type 'x_open_window' have two runners: {} and {}.",
            EXT_A_RUNNER.location(),
            EXT_DUP_RUNNER.location()
        )
    );

    let error = check_runners(&registry(&[&EXT_ORPHAN])).unwrap_err();
    assert_typed(
        &error,
        ErrorKind::Programming,
        &format!(
            "A runner for server actions of type 'x_orphan' is declared at {}, but \
             ir.actions.server.state has no such value: add it with selection_add.",
            EXT_ORPHAN_RUNNER.location()
        ),
        &ServerActionError::RunnerWithoutState {
            state: "x_orphan".to_owned(),
            location: EXT_ORPHAN_RUNNER.location(),
        },
    );

    let error = check_runners(&registry(&[&EXT_NORUNNER])).unwrap_err();
    assert_typed(
        &error,
        ErrorKind::Programming,
        "No runner for server actions of type 'x_norunner': the addon that adds the type must \
         declare one with server_action_runner!.",
        &ServerActionError::MissingRunner {
            state: "x_norunner".to_owned(),
        },
    );
}

/// The hook runs the same check on the final registry: the test harness has the state without
/// a runner, so `_register_hook` refuses it.
#[tokio::test]
async fn register_hook_checks_the_runners() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let error = erp_orm::model_hooks::call_register_hook(&mut tx, "ir.actions.server")
        .await
        .unwrap_err();
    assert_eq!(
        typed(&error),
        &ServerActionError::MissingRunner {
            state: "x_norunner".to_owned()
        }
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ----- audited run_as_superuser ---------------------------------------------

/// The audit rows of the database as `(events, subject_id, entry_kind)`, oldest first.
async fn audit_rows(client: &Client) -> Vec<(erp_fields::serde_json::Value, i64, String)> {
    client
        .query(
            "SELECT events::text, subject_id, entry_kind FROM erp_event.ir_audit_entry ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| {
            let events: String = row.get(0);
            (
                erp_fields::serde_json::from_str(&events).unwrap(),
                row.get(1),
                row.get(2),
            )
        })
        .collect()
}

/// member runs a flagged action as superuser and writes what the caller's rules refuse; one audit
/// row carries the observation with the action, its groups and who set the flag; a caller outside
/// the groups is refused and records nothing; a failure inside the subtree leaves no row; the
/// same action twice in one transaction is one entry with count 2; a flagged child of a flagged
/// action is its own entry. The unflagged twin of the action is refused by the caller's rules.
#[tokio::test]
async fn flagged_action_runs_as_superuser_and_is_audited() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let locked = record(
        &h,
        "x.partner",
        vals(&[("name", s("Locked")), ("city", s("Before"))]),
    )
    .await;
    let runner_group = || Command::link(rid(seeded.group_runner));
    let write_city = |name: &'static str, flagged: bool| {
        let v = vals(&[("update_path", s("city")), ("value", s("Bonn"))])
            .set_commands("group_ids", [runner_group()]);
        if flagged {
            v.set_cell("run_as_superuser", Cell::Bool(true))
        } else {
            v
        }
        .set_cell("sequence", Cell::Int(5))
        .set_cell("name", s(name))
    };
    let make = async |name: &'static str, state: &str, v: RawVals| -> i64 {
        action(
            &h,
            name,
            seeded.partner_model,
            state,
            v.set_cell("name", s(name)),
        )
        .await
    };
    let plain = make("Plain", "object_write", write_city("Plain", false)).await;
    let flagged = make("Flagged", "object_write", write_city("Flagged", true)).await;

    // the unflagged twin writes as the caller: refused by the caller's record rule
    let error = run(&orm, seeded.runner, plain, "x.partner", &[locked])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        read(&h, "x.partner", locked, "city").await,
        Value::str("Before")
    );
    assert!(audit_rows(&client).await.is_empty());

    // a caller outside the groups is refused before anything is recorded
    let error = run(&orm, seeded.demo, flagged, "x.partner", &[locked])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        read(&h, "x.partner", locked, "city").await,
        Value::str("Before")
    );
    assert!(audit_rows(&client).await.is_empty());

    // a group member runs the flagged action as superuser
    run(&orm, seeded.runner, flagged, "x.partner", &[locked])
        .await
        .unwrap();
    assert_eq!(
        read(&h, "x.partner", locked, "city").await,
        Value::str("Bonn")
    );
    let rows = audit_rows(&client).await;
    assert_eq!(rows.len(), 1, "one row per transaction");
    let (events, subject, kind) = &rows[0];
    assert_eq!((*subject, kind.as_str()), (seeded.runner, "change"));
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 1);
    let flag_writer: i64 = client
        .query_one(
            "SELECT write_uid::int8 FROM ir_act_server WHERE id = $1",
            &[&flagged],
        )
        .await
        .unwrap()
        .get(0);
    let e = &events[0];
    assert_eq!(e["type"], "elevation");
    assert_eq!(e["reason"], "base.server_action.run_as_superuser");
    assert_eq!(e["grant"]["kind"], "full");
    assert_eq!(e["grant"]["cause"]["kind"], "server_action");
    assert_eq!(e["grant"]["cause"]["action_id"], flagged);
    assert_eq!(e["grant"]["cause"]["root_action_id"], flagged);
    assert_eq!(
        e["grant"]["cause"]["group_ids"],
        erp_fields::serde_json::json!([seeded.group_runner])
    );
    assert_eq!(e["grant"]["cause"]["flag_write_uid"], flag_writer);
    assert!(e["grant"]["cause"]["flag_write_date_utc_micros"].is_i64());
    assert_eq!(e["original"]["subject"], seeded.runner);
    assert_eq!(e["effective_subject"], seeded.runner);
    assert_eq!(
        (e["count"].as_i64(), e["state"].as_str()),
        (Some(1), Some("effective"))
    );

    // a failure inside the subtree rolls everything back and records nothing
    let failing = make("Failing child", "object_write", {
        vals(&[
            ("update_path", s("name")),
            ("value", s("Renamed")),
            ("sequence", Cell::Int(9)),
        ])
        .set_commands("group_ids", [runner_group()])
    })
    .await;
    let parent = make(
        "Parent",
        "multi",
        RawVals::new()
            .set_commands("group_ids", [runner_group()])
            .set_commands("child_ids", [Command::set([rid(flagged), rid(failing)])]),
    )
    .await;
    let before = audit_rows(&client).await.len();
    let error = run(&orm, seeded.runner, parent, "x.partner", &[locked])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        audit_rows(&client).await.len(),
        before,
        "a rolled back run leaves no row"
    );
    assert_eq!(
        read(&h, "x.partner", locked, "name").await,
        Value::str("Locked")
    );

    // the same action twice in one transaction is one entry with count 2
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::new(UserId::new(seeded.runner).unwrap())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap();
    for _ in 0..2 {
        let env = tx.env();
        let ids = real(&[locked]);
        run_server_action(
            &mut tx,
            &env,
            &RunRequest::new(ActionId::new(flagged).unwrap(), "x.partner", &ids),
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let rows = audit_rows(&client).await;
    assert_eq!(rows.len(), before + 1);
    let events = rows.last().unwrap().0.clone();
    assert_eq!(events.as_array().unwrap().len(), 1);
    assert_eq!(events[0]["count"], 2);

    // a flagged child of a flagged action is its own entry, both with the run's root action
    let inner = make("Inner", "object_write", write_city("Inner", true)).await;
    let outer = make(
        "Outer",
        "multi",
        RawVals::new()
            .set_cell("run_as_superuser", Cell::Bool(true))
            .set_commands("group_ids", [runner_group()])
            .set_commands("child_ids", [Command::set([rid(inner)])]),
    )
    .await;
    run(&orm, seeded.runner, outer, "x.partner", &[locked])
        .await
        .unwrap();
    let events = audit_rows(&client).await.pop().unwrap().0;
    let actions: Vec<(i64, i64)> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["grant"]["cause"]["action_id"].as_i64().unwrap(),
                e["grant"]["cause"]["root_action_id"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(actions, [(outer, outer), (inner, outer)]);
    h.cleanup().await.unwrap();
}
