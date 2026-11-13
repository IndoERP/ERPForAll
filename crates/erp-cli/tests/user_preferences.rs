//! The My Preferences dialog on the installed `base` and `web` addons: the preferences action and form
//! records that `base` loads, and what an ordinary internal user can do with them through the
//! native RPC stack the browser client calls.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::sync::Arc;

use erp_fields::{Cell, RecId};
use erp_modules::AddonIndex;
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_orm::{Command, Orm, RawVals, SUPERUSER_ID};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcService};
use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

struct Host;
impl erp_rpc::RpcHost for Host {
    fn language_active(&self, code: &str) -> erp_fields::Result<bool> {
        Ok(code == "en_US")
    }
    fn has_capability(
        &self,
        _: &erp_orm::Principal,
        _: &erp_orm::Capability,
    ) -> erp_fields::Result<bool> {
        Ok(false)
    }
}

async fn base_xmlid(client: &tokio_postgres::Client, model: &str, name: &str) -> i64 {
    let rows = client
        .query(
            "SELECT res_id::bigint FROM ir_model_data WHERE module = 'base' AND model = $1 AND name = $2",
            &[&model, &name],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "base.{name} ({model}) must be loaded exactly once"
    );
    rows[0].get(0)
}

/// The nodes of a described form, by kind.
fn nodes_of<'a>(view: &'a serde_json::Value, kind: &str) -> Vec<&'a serde_json::Value> {
    view["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["kind"] == kind)
        .collect()
}

/// The kind of arena node `i` of a described view.
fn arena_kind(arena: &[serde_json::Value], i: u64) -> &str {
    arena[i as usize]["kind"].as_str().unwrap()
}

/// The child indexes of arena node `i`.
fn arena_kids(arena: &[serde_json::Value], i: u64) -> Vec<u64> {
    arena[i as usize]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| child.as_u64().unwrap())
        .collect()
}

/// Arena node `i`, which must be a field node.
fn arena_field(arena: &[serde_json::Value], i: u64) -> &serde_json::Value {
    assert_eq!(arena_kind(arena, i), "field");
    &arena[i as usize]
}

#[test]
fn installed_base_serves_the_preferences_dialog_to_an_internal_user() {
    // The module install plus native RPC chain needs more than the default test-thread stack
    // (same reason as `human_base_bootstrap.rs`).
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(preferences_end_to_end());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end walk over the dialog's server calls"
)]
async fn preferences_end_to_end() {
    let db = TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    // Explicit provenance for this binary's private native fixture bytes.
    let native_artifact_identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                std::path::Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(native_artifact_identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = system.initialize(InitOptions::new()).await.unwrap();
    // a database that serves audited calls carries the audit table; the
    // publication step installs it in a real deployment, this fixture does it directly.
    {
        let mut audit_tx = init
            .orm
            .begin_opts(
                erp_orm::BeginOptions::new(SUPERUSER_ID)
                    .isolation(erp_orm::Isolation::ReadCommitted)
                    .on_primary(),
            )
            .await
            .unwrap();
        audit_tx.install_audit_schema().await.unwrap();
        audit_tx.commit().await.unwrap();
    }
    assert!(init.data_notices.is_empty(), "{:?}", init.data_notices);
    assert!(init.post_commit.failed.is_empty());
    let client = db.client().await.unwrap();
    let main_company = base_xmlid(&client, "res.company", "main_company").await;
    let user_group = base_xmlid(&client, "res.groups", "group_user").await;
    let system_group = base_xmlid(&client, "res.groups", "group_system").await;
    let manager_group = base_xmlid(&client, "res.groups", "group_erp_manager").await;
    let action_id = base_xmlid(&client, "ir.actions.act_window", "action_res_users_my").await;
    let view_id = base_xmlid(&client, "ir.ui.view", "users_preferences_form").await;

    let source = PolicySource::build(Arc::clone(init.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let resolver = Resolver::for_registry(init.orm.registry()).unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(init.orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(init.orm.registry()),
            init.orm
                .env(SUPERUSER_ID, erp_orm::Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(resolver)
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let auth = Auth::new(
        orm.clone(),
        erp_security::auth::AuthConfig::DEFAULT,
        HashPool::global(),
    )
    .await
    .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let mut user_ids = Vec::new();
    // the base rules want an administrator; the two users under test are ordinary internal users
    for (login, group) in [
        ("prefs-admin", system_group),
        ("prefs-user", user_group),
        ("prefs-other", user_group),
        // Access Rights, not Settings: the shipped rows give this user no `res.users` row of
        // their own beyond the internal self row
        ("prefs-manager", manager_group),
    ] {
        let partner = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(login.into()))
                        .set_cell("company_id", Cell::Id(main_company)),
                ],
            )
            .await
            .unwrap();
        let created = tx
            .env()
            .records("res.users")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(login.into()))
                        .set_cell("login", Cell::Str(login.into()))
                        .set_cell("password", Cell::Str(format!("{login}-password").into()))
                        .set_cell("partner_id", Cell::Id(partner.ids()[0]))
                        .set_cell("company_id", Cell::Id(main_company))
                        .set_cell("lang", Cell::Str("en_US".into()))
                        .set_cell("tz", Cell::Str("UTC".into()))
                        .set_commands(
                            "company_ids",
                            [Command::set([RecId::from_real(main_company).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::set([RecId::from_real(group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        user_ids.push(created.ids()[0]);
    }
    tx.commit().await.unwrap();
    let (admin, me, other, manager) = (user_ids[0], user_ids[1], user_ids[2], user_ids[3]);

    let principal = async |login: &str| {
        let logged = auth
            .authenticate(LoginRequest::new(
                login,
                format!("{login}-password"),
                ClientAddr::InProcess("user-preferences-test"),
            ))
            .await
            .unwrap();
        let mut begun = auth
            .begin_session(logged.session().secret(), CredentialOptions::new())
            .await
            .unwrap();
        let principal = begun.tx.env().principal().clone();
        begun.tx.rollback().await.unwrap();
        principal
    };
    let ordinary = principal("prefs-user").await;
    let manager_principal = principal("prefs-manager").await;
    let admin_principal = principal("prefs-admin").await;
    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
    let call_as = async |who: &erp_orm::Principal, path: &str, body: serde_json::Value| {
        let response = service
            .call(RpcCall::json2(
                path,
                serde_json::to_vec(&body).unwrap(),
                Caller::Principal(who.clone()),
            ))
            .await;
        let json: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        (response.status(), json)
    };
    let call = async |path: &str, body: serde_json::Value| call_as(&ordinary, path, body).await;

    let (status, action) = call("/json/2/res.users/action_get", serde_json::json!({})).await;
    assert_eq!(status, 200, "{action}");
    assert_eq!(action["type"], "ir.actions.act_window");
    assert_eq!(action["id"], action_id);
    assert_eq!(action["res_model"], "res.users");
    assert_eq!(action["target"], "new");
    assert_eq!(action["view_mode"], "form");
    assert_eq!(action["views"], serde_json::json!([[view_id, "form"]]));
    assert!(!action["name"].as_str().unwrap().is_empty());

    let (status, description) = call(
        "/json/2/res.users/get_views",
        serde_json::json!({
            "views": [[view_id, "form"]],
            "wire_major": 1,
            "options": {"mobile": false},
        }),
    )
    .await;
    assert_eq!(status, 200, "{description}");
    assert!(description["notices"].as_array().unwrap().is_empty());
    let form = &description["views"][0];
    assert_eq!(form["type"], "form");
    let mut fields: Vec<(String, String)> = nodes_of(form, "field")
        .iter()
        .map(|node| {
            (
                node["field"]["name"].as_str().unwrap().to_owned(),
                node["widget"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    // the arena order is the layout's business (below); the set is the case's
    fields.sort();
    let mut expected_fields: Vec<(String, String)> = [
        ("name", "char"),
        ("email", "email"),
        ("phone", "phone"),
        ("lang", "web.preference_choice"),
        ("color_scheme", "radio"),
        ("tz", "web.preference_choice"),
    ]
    .into_iter()
    .map(|(name, widget)| (name.to_owned(), widget.to_owned()))
    .collect();
    expected_fields.sort();
    assert_eq!(fields, expected_fields);
    let buttons = nodes_of(form, "button");
    assert_eq!(buttons.len(), 2);
    assert_eq!(
        buttons
            .iter()
            .filter(|button| button["name"] == "preference_change_password")
            .count(),
        1
    );
    let save: Vec<_> = buttons
        .into_iter()
        .filter(|button| button["name"] == "preference_save")
        .collect();
    assert_eq!(save.len(), 1);
    assert_eq!(save[0]["name"], "preference_save");
    assert_eq!(save[0]["button_type"], "object");
    assert_eq!(save[0]["variant"], "primary");
    assert_eq!(save[0]["details"]["hotkey"], "q");
    let cancel = nodes_of(form, "cancel_button");
    assert_eq!(cancel.len(), 1);
    assert_eq!(cancel[0]["details"]["hotkey"], "x");
    assert_eq!(nodes_of(form, "footer").len(), 1);

    // receives. Group names are not on the wire (a group node carries label, columns, colspan),
    // so the names the extension points rely on are read from the stored arch below.
    let arena = form["nodes"].as_array().unwrap().as_slice();
    let kind = |i: u64| arena_kind(arena, i);
    let kids = |i: u64| arena_kids(arena, i);
    let kinds = |i: u64| kids(i).into_iter().map(kind).collect::<Vec<_>>();
    let field_at = |i: u64| arena_field(arena, i);
    let root = form["root"].as_u64().unwrap();
    assert_eq!(kind(root), "form");
    assert_eq!(kinds(root), ["sheet", "footer"]);
    let sheet = kids(root)[0];
    assert_eq!(kinds(sheet), ["title", "notebook"]);
    let title = kids(sheet)[0];
    assert_eq!(kinds(title), ["heading", "field", "field"]);
    let title_kids = kids(title);
    assert_eq!(arena[title_kids[0] as usize]["level"], 1);
    let heading_fields = kids(title_kids[0]);
    assert_eq!(heading_fields.len(), 1);
    assert_eq!(field_at(heading_fields[0])["field"]["name"], "name");
    assert_eq!(field_at(title_kids[1])["field"]["name"], "email");
    assert_eq!(field_at(title_kids[1])["widget"], "email");
    assert_eq!(field_at(title_kids[2])["field"]["name"], "phone");
    assert_eq!(field_at(title_kids[2])["widget"], "phone");
    let notebook = kids(sheet)[1];
    assert_eq!(kinds(notebook), ["page", "page"]);
    let pages = kids(notebook);
    assert_eq!(arena[pages[0] as usize]["name"], "preferences_page");
    assert_eq!(arena[pages[0] as usize]["label"], "Preferences");
    assert_eq!(arena[pages[1] as usize]["name"], "calendar");
    assert_eq!(arena[pages[1] as usize]["label"], "Calendar");
    // Preferences page: an outer group with a left group (language) and a right group (colour scheme)
    assert_eq!(kinds(pages[0]), ["group"]);
    let outer = kids(pages[0])[0];
    assert_eq!(kinds(outer), ["group", "group"]);
    let columns = kids(outer);
    let left = kids(columns[0]);
    assert_eq!(left.len(), 1);
    assert_eq!(field_at(left[0])["field"]["name"], "lang");
    let right = kids(columns[1]);
    assert_eq!(right.len(), 1);
    assert_eq!(field_at(right[0])["field"]["name"], "color_scheme");
    assert_eq!(field_at(right[0])["widget"], "radio");
    assert_eq!(field_at(right[0])["options"]["horizontal"], true);
    // Calendar page: an outer group holding one group with the time zone
    assert_eq!(kinds(pages[1]), ["group"]);
    let calendar_outer = kids(pages[1])[0];
    assert_eq!(kinds(calendar_outer), ["group"]);
    let calendar_group = kids(calendar_outer)[0];
    assert_eq!(kinds(calendar_group), ["field"]);
    assert_eq!(field_at(kids(calendar_group)[0])["field"]["name"], "tz");
    // the names an addon's xpath relies on are in the stored arch
    let arch: String = client
        .query_one("SELECT arch_db FROM ir_ui_view WHERE id = $1", &[&view_id])
        .await
        .unwrap()
        .get(0);
    for name in ["other_preferences", "calendar_preferences"] {
        assert!(
            arch.contains(&format!("<group name=\"{name}\"")),
            "the stored preferences arch has no group named {name}: {arch}"
        );
    }

    let (status, rows) = call(
        "/json/2/res.users/read",
        serde_json::json!({"ids": [me], "fields": ["name", "email", "phone", "lang", "color_scheme", "tz"]}),
    )
    .await;
    assert_eq!(status, 200, "{rows}");
    assert_eq!(rows[0]["lang"], "en_US");
    assert_eq!(rows[0]["tz"], "UTC");
    assert_eq!(rows[0]["color_scheme"], "system");
    let (status, foreign) = call(
        "/json/2/res.users/read",
        serde_json::json!({"ids": [other], "fields": ["name"]}),
    )
    .await;
    assert_eq!(status, 403, "{foreign}");
    assert_eq!(foreign["name"], "erp.exceptions.AccessError");

    let (status, described) = call(
        "/json/2/res.partner/fields_get",
        serde_json::json!({"allfields": ["lang", "tz"], "attributes": ["selection"]}),
    )
    .await;
    assert_eq!(status, 200, "{described}");
    assert_eq!(
        described["lang"]["selection"],
        serde_json::json!([["en_US", "English (US)"]])
    );
    let zones = described["tz"]["selection"].as_array().unwrap();
    assert!(zones.contains(&serde_json::json!(["Asia/Jakarta", "Asia/Jakarta"])));

    let (status, saved) = call(
        "/json/2/res.users/write",
        serde_json::json!({"ids": [me], "vals": {"name": "Prefs User", "tz": "Asia/Jakarta"}}),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    let (status, refused) = call(
        "/json/2/res.users/write",
        serde_json::json!({"ids": [me], "vals": {"lang": "id_ID"}}),
    )
    .await;
    assert_eq!(
        status, 422,
        "id_ID is in the catalogue but not installed: {refused}"
    );
    assert_eq!(refused["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        refused["message"],
        "Wrong value for res.users.lang: 'id_ID'"
    );
    let (status, refused) = call(
        "/json/2/res.users/write",
        serde_json::json!({"ids": [me], "vals": {"tz": "Mars/Olympus_Mons"}}),
    )
    .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(
        refused["message"],
        "Wrong value for res.users.tz: 'Mars/Olympus_Mons'"
    );
    let (status, refused) = call(
        "/json/2/res.users/write",
        serde_json::json!({"ids": [me], "vals": {"login": "someone-else"}}),
    )
    .await;
    assert_eq!(
        status, 403,
        "login is not a field a user may write: {refused}"
    );

    let (status, saved) = call(
        "/json/2/res.users/write",
        serde_json::json!({"ids": [me], "vals": {"phone": "+62 21 555 0100", "color_scheme": "dark"}}),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    let (status, rows) = call(
        "/json/2/res.users/read",
        serde_json::json!({"ids": [me], "fields": ["phone", "color_scheme"]}),
    )
    .await;
    assert_eq!(status, 200, "{rows}");
    assert_eq!(rows[0]["phone"], "+62 21 555 0100");
    assert_eq!(rows[0]["color_scheme"], "dark");
    let (status, refused) = call(
        "/json/2/res.users/write",
        serde_json::json!({"ids": [me], "vals": {"color_scheme": "sepia"}}),
    )
    .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        refused["message"],
        "Wrong value for res.users.color_scheme: 'sepia'"
    );
    let (status, refused) = call(
        "/json/2/res.users/write",
        serde_json::json!({"ids": [me], "vals": {"color_scheme": false}}),
    )
    .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["name"], "erp.exceptions.ValidationError");
    assert_eq!(refused["message"], "The color scheme cannot be empty.");
    let (status, foreign) = call(
        "/json/2/res.users/write",
        serde_json::json!({"ids": [other], "vals": {"phone": "000"}}),
    )
    .await;
    assert_eq!(status, 403, "{foreign}");
    assert_eq!(foreign["name"], "erp.exceptions.AccessError");
    let (status, ok) = call(
        "/json/2/res.users/write",
        serde_json::json!({"ids": [me], "vals": {"phone": false}}),
    )
    .await;
    assert_eq!(status, 200, "clearing the phone is accepted: {ok}");

    let (status, done) = call(
        "/json/2/res.users/preference_save",
        serde_json::json!({"ids": [me]}),
    )
    .await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(
        done,
        serde_json::json!({"type": "ir.actions.act_window_close"})
    );
    let (status, refused) = call(
        "/json/2/res.users/preference_save",
        serde_json::json!({"ids": [other]}),
    )
    .await;
    assert_eq!(status, 422, "{refused}");

    // The calls the browser makes for the dialog, as the internal user on the installed rows:
    // the record load, the onchange of an edit, the save, and the normalisation of the button's
    // action result (`addons/web/static/src/client/record.ts`, `app.ts`).
    let spec = serde_json::json!({
        "name": {}, "email": {}, "phone": {}, "lang": {}, "color_scheme": {}, "tz": {}
    });
    let (status, records) = call(
        "/json/2/res.users/web_read",
        serde_json::json!({"ids": [me], "specification": spec}),
    )
    .await;
    assert_eq!(status, 200, "{records}");
    assert_eq!(records[0]["id"], me);
    assert_eq!(records[0]["name"], "Prefs User");
    let (status, changed) = call(
        "/json/2/res.users/onchange",
        serde_json::json!({
            "ids": [me],
            "values": {
                "name": "Prefs User", "email": false, "phone": false, "lang": "en_US",
                "color_scheme": "dark", "tz": "Asia/Jakarta"
            },
            "field_names": ["tz"],
            "fields_spec": spec,
        }),
    )
    .await;
    assert_eq!(status, 200, "{changed}");
    assert!(changed["value"].is_object(), "{changed}");
    let (status, saved_rows) = call(
        "/json/2/res.users/web_save",
        serde_json::json!({
            "ids": [me],
            "vals": {"name": "Prefs User", "tz": "Asia/Jakarta"},
            "specification": spec,
        }),
    )
    .await;
    assert_eq!(status, 200, "{saved_rows}");
    assert_eq!(saved_rows[0]["id"], me);
    assert_eq!(saved_rows[0]["tz"], "Asia/Jakarta");
    let (status, normalized) = call(
        "/json/2/res.users/erp_normalize_action_result",
        serde_json::json!({
            "wire_major": 1,
            "result": {"type": "ir.actions.act_window_close"},
            "context": {},
        }),
    )
    .await;
    assert_eq!(status, 200, "{normalized}");

    // internal self row and no user-management row, so the dialog's fields work on their own
    // record and nothing else does
    let (status, ok) = call_as(
        &manager_principal,
        "/json/2/res.users/write",
        serde_json::json!({"ids": [manager], "vals": {
            "name": "Manager Renamed", "email": "manager@example.com",
            "phone": "+62 21 555 0199", "lang": "en_US", "color_scheme": "light",
            "tz": "Asia/Jakarta"}}),
    )
    .await;
    assert_eq!(status, 200, "{ok}");
    let foreign_partner: i64 = client
        .query_one(
            "SELECT partner_id::int8 FROM res_users WHERE id = $1",
            &[&other],
        )
        .await
        .unwrap()
        .get(0);
    for (field, value) in [
        ("password", serde_json::json!("taken-over")),
        ("login", serde_json::json!("prefs-manager-2")),
        ("partner_id", serde_json::json!(foreign_partner)),
        (
            "group_ids",
            serde_json::json!([[6, 0, [user_group, system_group]]]),
        ),
        ("active", serde_json::json!(true)),
    ] {
        let (status, refused) = call_as(
            &manager_principal,
            "/json/2/res.users/write",
            serde_json::json!({"ids": [manager], "vals": {"name": "Not Saved", field: value}}),
        )
        .await;
        assert_eq!(status, 403, "{field}: {refused}");
        assert_eq!(refused["name"], "erp.exceptions.AccessError", "{field}");
    }
    let row = client
        .query_one(
            "SELECT name, login, partner_id::int8 FROM res_users WHERE id = $1",
            &[&manager],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, String>(0),
        "Manager Renamed",
        "nothing was committed"
    );
    assert_eq!(row.get::<_, String>(1), "prefs-manager");
    assert_ne!(row.get::<_, i64>(2), foreign_partner);
    let groups: Vec<i64> = client
        .query(
            "SELECT gid::int8 FROM res_groups_users_rel WHERE uid = $1 ORDER BY gid",
            &[&manager],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(!groups.contains(&system_group), "{groups:?}");
    // the old password still works: the refused write set no new one
    auth.authenticate(LoginRequest::new(
        "prefs-manager",
        "prefs-manager-password",
        ClientAddr::InProcess("user-preferences-test"),
    ))
    .await
    .unwrap();
    // a member of Settings (base.group_system) still writes the protected fields
    let (status, ok) = call_as(
        &admin_principal,
        "/json/2/res.users/write",
        serde_json::json!({"ids": [admin], "vals": {"login": "prefs-admin"}}),
    )
    .await;
    assert_eq!(status, 200, "{ok}");

    let row = client
        .query_one("SELECT name, tz, lang FROM res_users WHERE id = $1", &[&me])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "Prefs User");
    assert_eq!(row.get::<_, String>(1), "Asia/Jakarta");
    assert_eq!(row.get::<_, String>(2), "en_US");
    let untouched: String = client
        .query_one("SELECT name FROM res_users WHERE id = $1", &[&other])
        .await
        .unwrap()
        .get(0);
    assert_eq!(untouched, "prefs-other");
}
