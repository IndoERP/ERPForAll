//! The host-access bridge under the compiled snapshot policy (design,
//! §9, §10, §21.3): a [`GuardedArch`] captured over the
//! real registry, the plan the bundle needs decided by `Transaction::evaluate_guards` in the
//! pinned `PolicySnapshot`, and `erp_views::host_access::filter` turning both into the allowed part
//! of the arch. Every refusal of the bridge is checked with its kind and message.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_fields::{Cell, Error, ErrorKind, Result, Value};
use erp_orm::{
    Env, FieldAccessCtx, FieldAccessImpl, FieldAccessSuper, FieldIdx, FieldMask, FieldOp, GroupId,
    GroupSpec, GuardIdx, GuardPlan, GuardPlanBuilder, GuardPlanLimits, Orm, PolicyProvider,
    RawVals, RecordGuardIdx, Transaction, UserId,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_views::host_access::{filter, plan_of};
use erp_views::{GuardLookup, GuardedArch, ViewCatalogue, ViewDescriptor, ViewLimits};
use support::{connect, pool};

static COUNTRY: AddonDescriptor = AddonDescriptor {
    name: "x_country",
    depends: &["base"],
    exact_only: false,
};

const COUNTRY_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::char("code").groups("base.group_system"),
    FieldDecl::char("cost"),
    FieldDecl::one2many("line_ids", "x.line", "country_id"),
];

const LINE_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("label"),
    FieldDecl::char("cost").groups("base.group_system"),
    FieldDecl::many2one("country_id", "x.country"),
];

/// The compiled policy's own `#[field_access]` override, registered for this test binary: `name`
/// of `x.country` is unreadable while the context says `fa_veil` (a declared, context-dependent
/// field verdict, like the `test_fa.veiled` fixture of `erp-test-models` but under the compiled
/// snapshot policy).
fn veil(
    ctx: &FieldAccessCtx<'_>,
    op: FieldOp,
    mask: FieldMask,
    sup: FieldAccessSuper<'_>,
) -> Result<FieldMask> {
    let mut mask = sup.call(ctx, op, mask)?;
    if op == FieldOp::Read && ctx.context("fa_veil")? == Some(&Value::Bool(true)) {
        mask.deny(ctx.field("name")?, FieldOp::Read)?;
    }
    Ok(mask)
}

#[erp_orm::linkme::distributed_slice(erp_orm::FIELD_ACCESS_IMPLS)]
#[linkme(crate = erp_orm::linkme)]
static VEIL: FieldAccessImpl = FieldAccessImpl {
    addon: "x_country",
    model: "x.country",
    method: "veil",
    context: &["fa_veil"],
    run: veil,
    file: file!(),
    line: line!(),
};

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
            .fields(COUNTRY_FIELDS),
        ModelDecl::new("x_country", "x.line")
            .description("Line")
            .fields(LINE_FIELDS),
    ])
    .build()
    .await
    .expect("harness")
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

struct Seeded {
    /// internal user, also in `base.group_no_one`
    demo: i64,
    /// internal user and `base.group_system`
    sys: i64,
    /// only in the restricted group, which implies `base.group_user`
    solo: i64,
    internal: i64,
    restricted: i64,
    system: i64,
    no_one: i64,
    country: i64,
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
    let system = group("Settings", "base.group_system").await;
    let no_one = group("Technical Features", "base.group_no_one").await;
    client
        .execute(
            "INSERT INTO res_groups_implied_rel (gid, hid) VALUES ($1, $2)",
            &[&restricted, &internal],
        )
        .await
        .unwrap();
    let user = async |name: &str, groups: &[i64]| -> i64 {
        let id = one(
            client,
            "INSERT INTO res_users (name, login, active, share) VALUES ($1, $1, true, false) \
             RETURNING id::int8",
            &[&name],
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
    let demo = user("demo", &[internal, no_one]).await;
    let sys = user("sys", &[internal, system]).await;
    let solo = user("solo", &[restricted]).await;
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
    let line = model("Line", "x.line").await;
    for model_id in [country, line] {
        client
            .execute(
                "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
                 VALUES ('read', $1, $2, 'r', NULL, true)",
                &[&model_id, &internal],
            )
            .await
            .unwrap();
    }
    Seeded {
        demo,
        sys,
        solo,
        internal,
        restricted,
        system,
        no_one,
        country,
    }
}

/// The compiled policy source and an ORM over it.
async fn policy_orm(h: &Harness) -> (Arc<PolicySource>, Orm) {
    let source = Arc::new(
        PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
            .await
            .unwrap(),
    );
    let provider: Arc<dyn PolicyProvider> = source.clone();
    let orm = Orm::builder(Arc::clone(h.registry()), pool(h, 2))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();
    (source, orm)
}

/// What the capture reads from the registry (the base addon's own lookup is private to it).
struct RegistryLookup<'r> {
    registry: &'r Registry,
    generation: u64,
}

impl GuardLookup for RegistryLookup<'_> {
    fn registry_generation(&self) -> u64 {
        self.generation
    }
    fn field_groups(&self, model: &str, field: &str) -> Option<&str> {
        self.registry.model(model)?.field(field)?.groups
    }
    fn comodel(&self, model: &str, field: &str) -> Option<&str> {
        self.registry.model(model)?.field(field)?.ty.comodel()
    }
    fn has_field(&self, model: &str, field: &str) -> bool {
        self.registry
            .model(model)
            .is_some_and(|m| m.field(field).is_some())
    }
}

/// A bundle of `xml` for `x.country`, captured under the registry's generation plus `skew`.
fn bundle_with(h: &Harness, xml: &str, record_guards: &[(i64, &[i64])], skew: u64) -> GuardedArch {
    let combined = ViewCatalogue::new(vec![ViewDescriptor::new(1, "v", "x.country", xml)])
        .expect("catalogue")
        .combine(1)
        .expect("combines");
    let lookup = RegistryLookup {
        registry: h.registry(),
        generation: h.registry().generation() + skew,
    };
    GuardedArch::capture(
        combined,
        "x.country",
        record_guards,
        &lookup,
        &ViewLimits::default(),
    )
    .expect("captures")
}

fn bundle(h: &Harness, xml: &str) -> GuardedArch {
    bundle_with(h, xml, &[], 0)
}

fn plan_for(g: &GuardedArch) -> Arc<GuardPlan> {
    Arc::new(plan_of(g).expect("plan"))
}

/// Mints the bundle's own plan for `uid` under the environment `adjust` makes and filters.
async fn serve(
    orm: &Orm,
    uid: i64,
    adjust: impl FnOnce(Env) -> Env,
    g: &GuardedArch,
) -> Result<String> {
    let mut tx = orm.begin(UserId::new(uid).unwrap()).await?;
    let env = adjust(tx.env());
    let plan = plan_for(g);
    let evaluation = tx.evaluate_guards(&env, &plan).await?;
    let out = filter(&env, g, &evaluation)?.to_xml();
    drop(evaluation);
    tx.commit().await?;
    Ok(out)
}

/// The error of a refused bridge call.
fn refusal<T>(result: Result<T>) -> Error {
    match result {
        Ok(_) => panic!("the bridge accepted what it must refuse"),
        Err(error) => error,
    }
}

fn refused(result: Result<String>, kind: ErrorKind) -> String {
    let error: Error = result.expect_err("is refused");
    assert_eq!(error.kind(), kind, "{error:?}");
    error.message().to_owned()
}

fn spec(text: &str) -> GroupSpec {
    GroupSpec::parse(text).unwrap()
}

/// anything else before a node is visited)
///
/// Another user is another principal, and so is the same user with the debug toggled.
#[tokio::test]
async fn an_evaluation_for_another_principal_is_refused() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let g = bundle(
        &h,
        "<form><field name=\"name\"/><div groups=\"base.group_user\"/></form>",
    );
    let plan = plan_for(&g);

    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let evaluation = tx.evaluate_guards(&env, &plan).await.unwrap();
    // the same environment passes (control)
    assert_eq!(
        filter(&env, &g, &evaluation).unwrap().to_xml(),
        "<form><field name=\"name\"/><div/></form>"
    );
    for other in [env.with_user(seeded.sys), env.with_debug(true)] {
        let error = refusal(filter(&other, &g, &evaluation));
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "guard evaluation was minted for another principal (or the environment's binding cannot be \
         built in this transaction)"
        );
    }
}

/// An internal user without the settings group is in superuser mode: the node guards still
/// answer on the user's own groups (the guarded `code` leaves), while the field masks are
/// all-readable (`name` stays although the context vetoes it for the plain environment).
#[tokio::test]
async fn superuser_mode_keeps_guards_and_opens_field_masks() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let g = bundle(
        &h,
        "<form><field name=\"name\"/><field name=\"code\"/><div groups=\"base.group_system\"/></form>",
    );
    let veiled = |env: Env| env.with_context([("fa_veil", Value::Bool(true))]);

    let plain = serve(&orm, seeded.demo, veiled, &g).await.unwrap();
    assert_eq!(
        plain, "<form></form>",
        "veiled: name unreadable, code and div guarded"
    );
    let sudo = serve(&orm, seeded.demo, |env| veiled(env).sudo(true), &g)
        .await
        .unwrap();
    assert_eq!(
        sudo, "<form><field name=\"name\"/></form>",
        "superuser: the mask is open, the guards still follow the user's groups"
    );
    let admin = serve(&orm, seeded.sys, |env| veiled(env).sudo(true), &g)
        .await
        .unwrap();
    assert_eq!(
        admin,
        "<form><field name=\"name\"/><field name=\"code\"/><div/></form>"
    );
}

/// Bundle X filtered with the evaluation of the plan of bundle Y (another guard table) is
/// refused naming the first position that differs; a bundle captured under another registry
/// generation is a stale derived artifact even when its XML is byte-equal.
#[tokio::test]
async fn an_evaluation_of_another_bundle_is_refused() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let x = bundle(
        &h,
        "<form><div groups=\"base.group_system\"/><field name=\"name\"/></form>",
    );
    let y = bundle(
        &h,
        "<form><div groups=\"base.group_user\"/><field name=\"name\"/></form>",
    );

    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let of_y = tx.evaluate_guards(&env, &plan_for(&y)).await.unwrap();
    let error = refusal(filter(&env, &x, &of_y));
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "guard evaluation does not belong to this view bundle (guard 0 differs)"
    );
    drop(of_y);

    // a model other than the bundle's root
    let mut b = GuardPlanBuilder::new("x.line", GuardPlanLimits::default()).unwrap();
    b.node_guard(spec("base.group_system")).unwrap();
    let other_root = Arc::new(b.finish().unwrap());
    let of_line = tx.evaluate_guards(&env, &other_root).await.unwrap();
    let error = refusal(filter(&env, &x, &of_line));
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "guard evaluation does not belong to this view bundle (root model: 'x.line' against \
         'x.country' differs)"
    );
}

/// A plan made of every guard of the bundle but the last one is decided by the transaction like
/// any other; the bridge sees that it is not the bundle's plan and names the difference.
#[tokio::test]
async fn a_plan_with_an_omitted_guard_is_refused() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let g = bundle(
        &h,
        "<form><div groups=\"base.group_system\"/><p groups=\"base.group_user\"/>\
         <field name=\"name\"/></form>",
    );
    let full = plan_of(&g).unwrap();
    assert_eq!(full.node_guards().len(), 2);

    let mut b = GuardPlanBuilder::new(full.root_model(), GuardPlanLimits::default()).unwrap();
    for guard in &full.node_guards()[..full.node_guards().len() - 1] {
        b.node_guard(guard.clone()).unwrap();
    }
    for field in full.fields() {
        b.field(field.model(), field.path()).unwrap();
    }
    let omitted = Arc::new(b.finish().unwrap());

    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let evaluation = tx.evaluate_guards(&env, &omitted).await.unwrap();
    let error = refusal(filter(&env, &g, &evaluation));
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "guard evaluation does not belong to this view bundle (guard count: 1 against 2 differs)"
    );
}

/// under)
#[tokio::test]
async fn a_bundle_of_another_registry_generation_is_stale() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let xml = "<form><field name=\"name\"/></form>";
    let current = bundle(&h, xml);
    let ahead = bundle_with(&h, xml, &[], 1);
    assert_eq!(
        current.arch().to_xml(),
        ahead.arch().to_xml(),
        "byte-equal XML"
    );

    assert_eq!(
        serve(&orm, seeded.demo, |env| env, &current).await.unwrap(),
        xml
    );
    let generation = h.registry().generation();
    let message = refused(
        serve(&orm, seeded.demo, |env| env, &ahead).await,
        ErrorKind::Validation,
    );
    assert_eq!(
        message,
        format!(
            "view bundle for registry generation {} served under generation {generation}; \
             refresh the 'templates' cache",
            generation + 1
        )
    );
}

/// old snapshot are dropped)
///
/// The settings user sees `code`. Their membership is removed and a policy row is committed by
/// another connection; after the shared snapshot is rebuilt the next transaction no longer
/// serves `code`, and the resolved plan cached for the old snapshot is invalidated (counted).
#[tokio::test]
async fn revocation_is_seen_and_the_old_snapshot_is_dropped() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let (source, orm) = policy_orm(&h).await;
    let g = bundle(
        &h,
        "<form><field name=\"name\"/><field name=\"code\"/></form>",
    );

    let before = serve(&orm, seeded.sys, |env| env, &g).await.unwrap();
    assert_eq!(
        before,
        "<form><field name=\"name\"/><field name=\"code\"/></form>"
    );
    let stats = orm.resolved_guard_cache_stats();
    assert_eq!(stats.entries, 1, "{stats:?}");

    client
        .execute(
            "DELETE FROM res_groups_users_rel WHERE uid = $1 AND gid = $2",
            &[&seeded.sys, &seeded.system],
        )
        .await
        .unwrap();
    // a policy row bumps the generation: the snapshot is rebuilt
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
             VALUES ('read again', $1, $2, 'r', NULL, true)",
            &[&seeded.country, &seeded.internal],
        )
        .await
        .unwrap();
    source.refresh_shared().await.unwrap();

    let after = serve(&orm, seeded.sys, |env| env, &g).await.unwrap();
    assert_eq!(after, "<form><field name=\"name\"/></form>");
    let later = orm.resolved_guard_cache_stats();
    assert_eq!(
        later.invalidated - stats.invalidated,
        1,
        "the plan resolved under the old snapshot is gone: {stats:?} then {later:?}"
    );
    assert_eq!(
        later.entries, 1,
        "only the new snapshot's resolution is held"
    );
}

/// mask and a context-dependent override of the compiled policy)
///
/// `code` is restricted to the settings group; `name` is vetoed by the override while the context
/// says `fa_veil`. Each is removed with what it contains, its tail text stays.
#[tokio::test]
async fn unreadable_fields_are_removed_with_their_subtree() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let g = bundle(
        &h,
        "<form><field name=\"name\"><b>inner</b></field>after name \
         <field name=\"code\"><i/></field>after code</form>",
    );
    let open = serve(&orm, seeded.demo, |env| env, &g).await.unwrap();
    assert_eq!(
        open,
        "<form><field name=\"name\"><b>inner</b></field>after name after code</form>"
    );
    let veiled = serve(
        &orm,
        seeded.demo,
        |env| env.with_context([("fa_veil", Value::Bool(true))]),
        &g,
    )
    .await
    .unwrap();
    assert_eq!(veiled, "<form>after name after code</form>");
    let admin = serve(&orm, seeded.sys, |env| env, &g).await.unwrap();
    assert_eq!(
        admin,
        "<form><field name=\"name\"><b>inner</b></field>after name \
         <field name=\"code\"><i/></field>after code</form>"
    );
}

/// `cost` is open on `x.country` and restricted on `x.line`: the same name in the sub-view of
/// `line_ids` is judged by the comodel and removed, the root's own `cost` stays. The plan names
/// both fields by their model.
#[tokio::test]
async fn a_subview_field_is_judged_by_the_comodel() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let g = bundle(
        &h,
        "<form><field name=\"cost\"/><field name=\"line_ids\"><list>\
         <field name=\"label\"/><field name=\"cost\"/></list></field></form>",
    );
    let fields: Vec<(String, String)> = plan_of(&g)
        .unwrap()
        .fields()
        .iter()
        .map(|f| (f.model().to_owned(), f.path().to_owned()))
        .collect();
    assert!(
        fields.contains(&("x.country".to_owned(), "cost".to_owned())),
        "{fields:?}"
    );
    assert!(
        fields.contains(&("x.line".to_owned(), "cost".to_owned())),
        "{fields:?}"
    );

    let demo = serve(&orm, seeded.demo, |env| env, &g).await.unwrap();
    assert_eq!(
        demo,
        "<form><field name=\"cost\"/><field name=\"line_ids\"><list>\
         <field name=\"label\"/></list></field></form>"
    );
    let admin = serve(&orm, seeded.sys, |env| env, &g).await.unwrap();
    assert_eq!(
        admin,
        "<form><field name=\"cost\"/><field name=\"line_ids\"><list>\
         <field name=\"label\"/><field name=\"cost\"/></list></field></form>"
    );
}

/// The user may read `x.country` but not `x.line`: the fields of the `line_ids` sub-view are
/// unreadable and leave (the readable `line_ids` field itself stays), the rest of the view is served, and no error is
/// raised for the comodel. Superuser mode keeps the whole view. A denied ROOT model stays a hard
/// `Access` error.
#[tokio::test]
async fn an_unreadable_comodel_removes_its_subview_and_is_not_an_error() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let line = one(
        &client,
        "SELECT id::int8 FROM ir_model WHERE model = 'x.line'",
        &[],
    )
    .await;
    client
        .execute("DELETE FROM ir_access WHERE model_id = $1", &[&line])
        .await
        .unwrap();
    let (_source, orm) = policy_orm(&h).await;
    let g = bundle(
        &h,
        "<form><field name=\"name\"/><field name=\"line_ids\"><list>\
         <field name=\"label\"/></list></field>tail</form>",
    );
    let demo = serve(&orm, seeded.demo, |env| env, &g).await.unwrap();
    // `line_ids` itself is a readable field of the root; the fields inside its sub-view belong to
    // the unreadable comodel and leave
    assert_eq!(
        demo,
        "<form><field name=\"name\"/><field name=\"line_ids\"><list></list></field>tail</form>"
    );
    let sudo = serve(&orm, seeded.demo, |env| env.sudo(true), &g)
        .await
        .unwrap();
    assert_eq!(
        sudo,
        "<form><field name=\"name\"/><field name=\"line_ids\"><list>\
         <field name=\"label\"/></list></field>tail</form>"
    );
    // the root model stays a hard refusal
    client
        .execute(
            "DELETE FROM ir_access WHERE model_id = $1",
            &[&seeded.country],
        )
        .await
        .unwrap();
    let (_source, orm) = policy_orm(&h).await;
    refused(
        serve(&orm, seeded.demo, |env| env, &g).await,
        ErrorKind::Access,
    );
}

/// The user may read `x.country` but not `x.line`. A sub-view container whose `decoration-*`
/// reads a `x.line` field, an inner node of the sub-view that does, and a `domain` on the
/// relational field are removed (the field with its `domain`, the container, the inner node);
/// the rest of the view is served with no error. Superuser mode keeps everything. A node outside
/// the sub-view that reads a field the user may not read stays a hard `Access` error.
#[tokio::test]
async fn an_unreadable_comodel_removes_the_expression_readers_of_its_subview() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let line = one(
        &client,
        "SELECT id::int8 FROM ir_model WHERE model = 'x.line'",
        &[],
    )
    .await;
    client
        .execute("DELETE FROM ir_access WHERE model_id = $1", &[&line])
        .await
        .unwrap();
    let (_source, orm) = policy_orm(&h).await;

    // the container's own decoration reads `label` of `x.line`
    let decorated = bundle(
        &h,
        "<form><field name=\"name\"/><field name=\"line_ids\">\
         <list decoration-muted=\"label == 'x'\"><field name=\"label\"/></list></field>tail</form>",
    );
    let demo = serve(&orm, seeded.demo, |env| env, &decorated)
        .await
        .unwrap();
    assert_eq!(
        demo,
        "<form><field name=\"name\"/><field name=\"line_ids\"></field>tail</form>"
    );
    let sudo = serve(&orm, seeded.demo, |env| env.sudo(true), &decorated)
        .await
        .unwrap();
    assert_eq!(sudo, decorated.arch().to_xml());

    // an inner node of the sub-view that reads `label`
    let inner = bundle(
        &h,
        "<form><field name=\"name\"/><field name=\"line_ids\"><form>\
         <div invisible=\"label\"><b/></div><field name=\"label\"/></form></field>tail</form>",
    );
    let demo = serve(&orm, seeded.demo, |env| env, &inner).await.unwrap();
    assert_eq!(
        demo,
        "<form><field name=\"name\"/><field name=\"line_ids\"><form></form></field>tail</form>"
    );
    let sudo = serve(&orm, seeded.demo, |env| env.sudo(true), &inner)
        .await
        .unwrap();
    assert_eq!(sudo, inner.arch().to_xml());

    // a `domain` on the relational field reads `x.line.label`
    let domained = bundle(
        &h,
        "<form><field name=\"name\"/><field name=\"line_ids\" \
         domain=\"[('label', '=', 'x')]\"><list><field name=\"label\"/></list></field>tail</form>",
    );
    let demo = serve(&orm, seeded.demo, |env| env, &domained)
        .await
        .unwrap();
    assert_eq!(demo, "<form><field name=\"name\"/>tail</form>");
    let sudo = serve(&orm, seeded.demo, |env| env.sudo(true), &domained)
        .await
        .unwrap();
    assert_eq!(sudo, domained.arch().to_xml());

    // a node outside the sub-view reading an unreadable field of the readable root still fails
    let veiled = bundle(
        &h,
        "<form>\n<div invisible=\"name == 'x'\"><b/></div><field name=\"line_ids\">\
         <list decoration-muted=\"label\"/></field></form>",
    );
    let message = refused(
        serve(
            &orm,
            seeded.demo,
            |env| env.with_context([("fa_veil", Value::Bool(true))]),
            &veiled,
        )
        .await,
        ErrorKind::Access,
    );
    assert_eq!(
        message,
        "view 1, line 2: the invisible attribute reads the field 'name' of model 'x.country', \
         which the user may not read"
    );
}

/// A denied field inside text keeps the text around it, a denied `<t>` leaves with its subtree
/// and its tail joins what precedes it, an allowed attribute-less `<t>` is unwrapped, and one with
/// attributes stays.
#[tokio::test]
async fn text_and_tails_survive_removal_and_unwrapping() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let g = bundle(
        &h,
        "<form><span>(<field name=\"code\"/>)</span>\
         <t groups=\"base.group_system\"><b>hidden</b></t>tail one \
         <t groups=\"base.group_user\"><field name=\"name\"/></t>\
         <t groups=\"base.group_user\" t-if=\"x\">kept</t></form>",
    );
    let demo = serve(&orm, seeded.demo, |env| env, &g).await.unwrap();
    assert_eq!(
        demo,
        "<form><span>()</span>tail one <field name=\"name\"/>\
         <t t-if=\"x\">kept</t></form>"
    );
}

/// A record guard on the restricted group: a user outside it gets `Access` naming the view id, a
/// member gets the whole arch. Membership comes from the compiled policy's resolution, and the
/// implied group counts (`solo` is only in the restricted group).
#[tokio::test]
async fn a_view_limited_to_groups_is_refused_to_others() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let (_source, orm) = policy_orm(&h).await;
    let xml = "<form><field name=\"name\"/></form>";
    let restricted = [seeded.restricted];
    let g = bundle_with(&h, xml, &[(7, &restricted)], 0);

    let message = refused(
        serve(&orm, seeded.demo, |env| env, &g).await,
        ErrorKind::Access,
    );
    assert_eq!(message, "view 7 is restricted to groups the user is not in");
    assert_eq!(serve(&orm, seeded.solo, |env| env, &g).await.unwrap(), xml);
    // the group of the view that the user holds only through an implication is enough as well
    let internal = [seeded.internal];
    let implied = bundle_with(&h, xml, &[(8, &internal)], 0);
    assert_eq!(
        serve(&orm, seeded.solo, |env| env, &implied).await.unwrap(),
        xml
    );
    // membership gained in the database is seen by the next transaction
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&seeded.demo, &seeded.restricted],
        )
        .await
        .unwrap();
    assert_eq!(serve(&orm, seeded.demo, |env| env, &g).await.unwrap(), xml);
}

/// under the compiled snapshot policy and through the bridge) — the GA-1 round-3 SHOULD for G1
///
/// Minted under the open context `name` is readable, and the evaluation is not for the same
/// principal under `fa_veil` (the bridge refuses it with `Access`); minted under `fa_veil` it is
/// unreadable and the arch loses the field. Controls: an undeclared context key is not identity;
/// another user and the debug mode are.
#[tokio::test]
async fn the_declared_field_context_is_identity_under_the_compiled_policy() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let g = bundle(&h, "<form><field name=\"name\"/></form>");
    let plan = plan_for(&g);
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let open = tx.env();
    let veiled = open.with_context([("fa_veil", Value::Bool(true))]);

    let evaluation = tx.evaluate_guards(&open, &plan).await.unwrap();
    assert!(
        evaluation
            .field_readable(FieldIdx::new(0).unwrap())
            .unwrap()
    );
    assert!(evaluation.is_for(&open));
    assert!(!evaluation.is_for(&veiled), "fa_veil changes the verdict");
    assert!(evaluation.is_for(&open.with_context([("lang", Value::str("fr_FR"))])));
    assert!(!evaluation.is_for(&open.with_user(seeded.sys)));
    assert!(!evaluation.is_for(&open.with_debug(true)));
    assert_eq!(
        filter(&open, &g, &evaluation).unwrap().to_xml(),
        "<form><field name=\"name\"/></form>"
    );
    let error = refusal(filter(&veiled, &g, &evaluation));
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "guard evaluation was minted for another principal (or the environment's binding cannot be \
         built in this transaction)"
    );
    drop(evaluation);

    let evaluation = tx.evaluate_guards(&veiled, &plan).await.unwrap();
    assert!(
        !evaluation
            .field_readable(FieldIdx::new(0).unwrap())
            .unwrap()
    );
    assert!(evaluation.is_for(&veiled));
    assert!(!evaluation.is_for(&open));
    assert_eq!(
        filter(&veiled, &g, &evaluation).unwrap().to_xml(),
        "<form></form>"
    );
}

/// compiled snapshot policy) — the GA-1 round-2 residual item
///
/// For every group a node guard naming it and a record guard on it give the same answer, for each
/// user and with the debug mode off and on. `solo` holds `base.group_user` only through the
/// implication of the restricted group, so the closure is what both read.
#[tokio::test]
async fn node_guards_and_record_guards_agree_under_the_compiled_policy() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let groups = [
        ("base.group_user", seeded.internal),
        ("x_country.group_restricted", seeded.restricted),
        ("base.group_system", seeded.system),
        ("base.group_no_one", seeded.no_one),
    ];
    let mut b = GuardPlanBuilder::new("x.country", GuardPlanLimits::default()).unwrap();
    for (n, (xmlid, id)) in groups.iter().enumerate() {
        b.node_guard(spec(xmlid)).unwrap();
        b.record_guard(n as i64 + 1, &[GroupId::new(*id).unwrap()])
            .unwrap();
    }
    let plan = Arc::new(b.finish().unwrap());

    let mut seen_true = 0;
    let mut seen_false = 0;
    for uid in [seeded.demo, seeded.sys, seeded.solo] {
        for debug in [false, true] {
            let mut tx = orm.begin(UserId::new(uid).unwrap()).await.unwrap();
            let env = tx.env().with_debug(debug);
            let evaluation = tx.evaluate_guards(&env, &plan).await.unwrap();
            for (n, (xmlid, _)) in groups.iter().enumerate() {
                let node = evaluation.node_allowed(GuardIdx::new(n).unwrap()).unwrap();
                let record = evaluation
                    .record_allowed(RecordGuardIdx::new(n).unwrap())
                    .unwrap();
                assert_eq!(node, record, "{xmlid} for user {uid}, debug = {debug}");
                if node {
                    seen_true += 1;
                } else {
                    seen_false += 1;
                }
            }
        }
    }
    assert!(seen_true > 0 && seen_false > 0, "{seen_true}/{seen_false}");
}

fn text(value: &str) -> Cell {
    Cell::Str(Arc::from(value))
}

/// Creates a form view of `x.country` named `name` with `arch` in `tx`.
async fn create_view(tx: &mut Transaction, name: &str, arch: &str) -> Result<i64> {
    let vals = [
        ("name", text(name)),
        ("model", text("x.country")),
        ("type", text("form")),
        ("arch", text(arch)),
    ]
    .into_iter()
    .fold(RawVals::new(), |v, (k, c)| v.set_cell(k, c));
    let views = tx.env().records("ir.ui.view")?;
    Ok(views.create(tx, vec![vals]).await?.ids()[0])
}

/// The error of creating the view in a transaction of its own that is dropped.
async fn view_refusal(h: &Harness, name: &str, arch: &str) -> Error {
    let mut tx = h.begin().await.unwrap();
    let error = create_view(&mut tx, name, arch)
        .await
        .expect_err("the view is refused");
    tx.rollback().await.unwrap();
    error
}

async fn view_accepted(h: &Harness, name: &str, arch: &str) {
    let mut tx = h.begin().await.unwrap();
    create_view(&mut tx, name, arch)
        .await
        .unwrap_or_else(|e| panic!("{name} refused: {e}"));
    tx.rollback().await.unwrap();
}

/// guard does not imply; the same groups, an enclosing node's and an implying group pass)
///
/// `x.country.code` is restricted to `base.group_system`. A manager group that implies it is seeded
/// through the implication relation. The check runs where views are written, with the groups and
/// their implications read from the database.
#[tokio::test]
async fn install_time_refuses_a_node_that_reads_a_field_it_does_not_guard() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let manager = one(
        &client,
        "INSERT INTO res_groups (name) VALUES ('Manager') RETURNING id::int8",
        &[],
    )
    .await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('x_country', 'group_manager', 'res.groups', $1)",
            &[&manager],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO res_groups_implied_rel (gid, hid) VALUES ($1, $2)",
            &[&manager, &seeded.system],
        )
        .await
        .unwrap();

    let message = |line: u32, name: &str| {
        format!(
            "Error while validating view {name}, arch line {line}:\nThe invisible attribute reads \
             the field \"code\" of model \"x.country\", which is restricted to the groups \
             \"base.group_system\"; a visible node may not read a field it does not guard: repeat \
             the field's groups on the node"
        )
    };
    let error = view_refusal(
        &h,
        "t10_open",
        "<form>\n<div invisible=\"code == 'x'\"/></form>",
    )
    .await;
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), message(2, "t10_open"));

    let error = view_refusal(
        &h,
        "t10_wrong_group",
        "<form>\n<div groups=\"base.group_user\" invisible=\"code == 'x'\"/></form>",
    )
    .await;
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), message(2, "t10_wrong_group"));

    for (name, arch) in [
        (
            "t10_same",
            "<form><div groups=\"base.group_system\" invisible=\"code == 'x'\"/></form>",
        ),
        (
            "t10_enclosing",
            "<form><group groups=\"base.group_system\"><div invisible=\"code\"/></group></form>",
        ),
        (
            "t10_implying",
            "<form><div groups=\"x_country.group_manager\" invisible=\"code\"/></form>",
        ),
        // a field that is not restricted needs no guard
        ("t10_plain", "<form><div invisible=\"name == 'x'\"/></form>"),
    ] {
        view_accepted(&h, name, arch).await;
    }
}

/// fails the request with `Access` naming view, line, attribute, model and field; superuser mode
/// and a user who may read the field are served)
///
/// The override of the compiled policy vetoes `x.country.name` while the context says `fa_veil`
/// (an overlay that denies a field after the view was installed).
#[tokio::test]
async fn serve_time_refuses_a_visible_node_reading_an_unreadable_field() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let (_source, orm) = policy_orm(&h).await;
    let g = bundle(
        &h,
        "<form>\n<div invisible=\"name == 'x'\"><b/></div></form>",
    );
    let veiled = |env: Env| env.with_context([("fa_veil", Value::Bool(true))]);

    let open = serve(&orm, seeded.demo, |env| env, &g).await.unwrap();
    assert_eq!(open, g.arch().to_xml());

    let message = refused(
        serve(&orm, seeded.demo, veiled, &g).await,
        ErrorKind::Access,
    );
    assert_eq!(
        message,
        "view 1, line 2: the invisible attribute reads the field 'name' of model 'x.country', \
         which the user may not read"
    );
    let sudo = serve(&orm, seeded.demo, |env| veiled(env).sudo(true), &g)
        .await
        .unwrap();
    assert_eq!(sudo, g.arch().to_xml());

    // a guarded node that is denied raises nothing: it is removed with what it reads
    let guarded = bundle(
        &h,
        "<form><div groups=\"base.group_system\" invisible=\"name\"/></form>",
    );
    let out = serve(&orm, seeded.demo, veiled, &guarded).await.unwrap();
    assert_eq!(out, "<form></form>");
}
