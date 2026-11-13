// SPDX-License-Identifier: MIT OR Apache-2.0
//! The My Preferences dialog with `mail` installed: `mail` adds
//! the notification choice to the dialog's `other_preferences` group by a view that inherits
//! `base.users_preferences_form`, and the field refuses the in-application value
//! for share users. The database is initialized with `base` only first, so the
//! same form is described before and after `mail` is installed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use std::collections::BTreeMap;

use erp_fields::{Cell, RecId, Value};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Orm, RawVals, Records, SUPERUSER_ID, Transaction, UserId};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcService};
use erp_sdk::ErrorKind;
use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

/// One field of a write.
fn one(field: &str, value: Value) -> BTreeMap<String, Value> {
    BTreeMap::from([(field.to_owned(), value)])
}

/// A write that sets the user's groups to exactly `group`.
fn only_group(field: &str, group: i64) -> BTreeMap<String, Value> {
    one(
        field,
        Value::list([Value::list([
            Value::Int(6),
            Value::Int(0),
            Value::list([Value::Int(group)]),
        ])]),
    )
}

const SHARE_REFUSAL: &str = "A portal or public user is notified by e-mail only; choose E-mail.";

fn repo_addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

fn system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(repo_addons());
    let native_artifact_identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    ModuleSystem::builder(db.pool().clone(), index)
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(native_artifact_identities)
        .code([
            &erp_addon_uom::ADDON,
            &erp_addon_base::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_product_mail::ADDON,
            &erp_addon_web::ADDON,
        ])
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .build()
        .unwrap()
}

fn big_stack(body: impl std::future::Future<Output = ()> + Send + 'static) {
    thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(body);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// The id of the record `base.<name>` of `model`.
async fn base_id(tx: &mut Transaction, model: &str, name: &str) -> i64 {
    let rows = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data \
             WHERE module = 'base' AND model = $1::text AND name = $2::text",
            &[&model, &name],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "base.{name} ({model}) must be loaded once");
    rows[0].get(0)
}

/// A user in one group, created by the superuser; `notification` is the choice to give, if any.
async fn create_user(
    tx: &mut Transaction,
    login: &str,
    groups: &[i64],
    notification: Option<&str>,
) -> erp_fields::Result<Records> {
    let company = base_id(tx, "res.company", "main_company").await;
    let partner = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(login.into()))
                    .set_cell("company_id", Cell::Id(company)),
            ],
        )
        .await?;
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(login.into()))
        .set_cell("login", Cell::Str(login.into()))
        .set_cell("password", Cell::Str(format!("{login}-password").into()))
        .set_cell("partner_id", Cell::Id(partner.ids()[0]))
        .set_cell("company_id", Cell::Id(company))
        .set_commands(
            "company_ids",
            [Command::Set(vec![RecId::from_real(company).unwrap()])],
        )
        .set_commands(
            "group_ids",
            [Command::Set(
                groups
                    .iter()
                    .map(|g| RecId::from_real(*g).unwrap())
                    .collect(),
            )],
        );
    if let Some(choice) = notification {
        vals = vals.set_cell("notification_type", Cell::Str(choice.into()));
    }
    tx.env()
        .records("res.users")
        .unwrap()
        .create(tx, vec![vals])
        .await
}

/// An administrator, so group changes of other users pass the last-administrator rule.
async fn ensure_administrator(orm: &Orm) -> i64 {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let system_group = base_id(&mut tx, "res.groups", "group_system").await;
    let created = create_user(&mut tx, "prefs_mail_admin", &[system_group], None)
        .await
        .unwrap();
    let id = created.ids()[0];
    tx.commit().await.unwrap();
    id
}

/// `get_views` of the preferences form, as the client asks for it, for an administrator.
async fn preferences_form(orm: &Orm, db: &TestDb, admin: i64) -> serde_json::Value {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let view_id = base_id(&mut tx, "ir.ui.view", "users_preferences_form").await;
    tx.rollback().await.unwrap();
    let source =
        erp_security::source::PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
            .await
            .unwrap();
    let published_views = orm
        .env(SUPERUSER_ID, erp_orm::Context::new())
        .generation_attachment::<erp_views::PublishedViewTypes>()
        .unwrap();
    let runtime_orm = Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(orm.registry()),
            published_views,
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(erp_security::resolver::Resolver::for_registry(orm.registry()).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let mut tx = runtime_orm
        .begin(UserId::new(admin).unwrap())
        .await
        .unwrap();
    let env = tx.env().clone();
    let described = erp_addon_web::metadata_test_support::test_get_views(
        &env,
        &mut tx,
        "res.users",
        vec![Value::list([Value::Int(view_id), Value::str("form")])],
        32 * 1024 * 1024,
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    serde_json::to_value(described).unwrap()
}

/// `(field name, widget)` of every field node of a described form.
fn field_nodes(form: &serde_json::Value) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = form["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["kind"] == "field")
        .map(|node| {
            (
                node["field"]["name"].as_str().unwrap().to_owned(),
                node["widget"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    fields.sort();
    fields
}

/// The names of the field children of the group node that holds `field`.
fn sibling_fields(form: &serde_json::Value, field: &str) -> Vec<String> {
    let arena = form["nodes"].as_array().unwrap();
    let holder = arena
        .iter()
        .find(|node| {
            node["kind"] == "group"
                && node["children"].as_array().unwrap().iter().any(|child| {
                    let child = &arena[child.as_u64().unwrap() as usize];
                    child["kind"] == "field" && child["field"]["name"] == field
                })
        })
        .unwrap_or_else(|| panic!("no group holds the field {field}"));
    holder["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| {
            let child = &arena[child.as_u64().unwrap() as usize];
            assert_eq!(child["kind"], "field", "{child}");
            child["field"]["name"].as_str().unwrap().to_owned()
        })
        .collect()
}

#[test]
fn mail_extends_the_preferences_dialog_with_the_notification_choice() {
    big_stack(async move {
        let db = TestDb::create().await.unwrap();
        let before_system = system(&db);
        let init = before_system
            .initialize(InitOptions::new().with_skip_auto_install(true))
            .await
            .unwrap();
        let orm_before = init.orm;
        let admin = ensure_administrator(&orm_before).await;
        let before = preferences_form(&orm_before, &db, admin).await;
        let before_fields = field_nodes(&before["views"][0]);
        assert!(
            !before_fields
                .iter()
                .any(|(name, _)| name == "notification_type"),
            "without mail the dialog has no notification choice: {before_fields:?}"
        );

        let system = system(&db);
        let env = system.system_env(erp_orm::Context::new());
        let outcome = system
            .button_immediate_install(&env, &[ModuleName::new("mail").unwrap()])
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean());
        let after = preferences_form(&outcome.orm, &db, admin).await;
        let form = &after["views"][0];
        // nothing else changed: the same fields plus the one mail adds
        let mut expected = before_fields.clone();
        expected.push(("notification_type".to_owned(), "radio".to_owned()));
        expected.sort();
        assert_eq!(field_nodes(form), expected);
        // it sits in the group that holds the colour scheme (`other_preferences`), after it
        assert_eq!(
            sibling_fields(form, "notification_type"),
            ["color_scheme", "notification_type"]
        );
        let node = form["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["kind"] == "field" && node["field"]["name"] == "notification_type")
            .unwrap();
        assert_eq!(node["options"]["horizontal"], true);
        // the extension is a view record of mail that inherits the base view; base is untouched
        let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
        let rows = tx
            .query(
                "SELECT v.arch_db, p.arch_db FROM ir_ui_view v \
                   JOIN ir_ui_view p ON p.id = v.inherit_id \
                   JOIN ir_model_data d ON d.res_id = p.id AND d.model = 'ir.ui.view' \
                  WHERE d.module = 'base' AND d.name = 'users_preferences_form' \
                    AND v.model = 'res.users'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "one view inherits base.users_preferences_form"
        );
        let extension: String = rows[0].get(0);
        let base_arch: String = rows[0].get(1);
        assert!(extension.contains("other_preferences"), "{extension}");
        assert!(!base_arch.contains("notification_type"), "{base_arch}");
        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}

#[test]
fn the_notification_choice_is_stored_and_refused_for_share_users() {
    big_stack(async move {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let init = system
            .initialize(InitOptions::new().with_skip_auto_install(true))
            .await
            .unwrap();
        let env = system.system_env(erp_orm::Context::new());
        let outcome = system
            .button_immediate_install(&env, &[ModuleName::new("mail").unwrap()])
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean());
        drop(init);
        let orm = outcome.orm;
        ensure_administrator(&orm).await;
        let (internal, portal) = {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let ids = (
                base_id(&mut tx, "res.groups", "group_user").await,
                base_id(&mut tx, "res.groups", "group_portal").await,
            );
            tx.rollback().await.unwrap();
            ids
        };
        // the default comes from the model's own metadata, not from a literal
        let default = {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let defaults = tx
                .env()
                .records("res.users")
                .unwrap()
                .default_get(&mut tx, &["notification_type"])
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            defaults.get("notification_type").unwrap().clone()
        };
        let value_of = async |tx: &mut Transaction, user: &Records| {
            user.value(tx, "notification_type").await.unwrap()
        };

        // an internal user: the default, both values, and the selection field's own refusal
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let user = create_user(&mut tx, "prefs_mail_internal", &[internal], None)
            .await
            .unwrap();
        assert_eq!(value_of(&mut tx, &user).await, default);
        for choice in ["inbox", "email"] {
            user.write_values(&mut tx, &one("notification_type", Value::str(choice)))
                .await
                .unwrap();
            assert_eq!(value_of(&mut tx, &user).await, Value::str(choice));
        }
        let err = user
            .write_values(&mut tx, &one("notification_type", Value::str("sms")))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            "Wrong value for res.users.notification_type: 'sms'"
        );
        tx.rollback().await.unwrap();
        // a rolled-back transaction keeps its connection until it is dropped, and the test
        // pool has four: each one below is dropped before the next begins
        drop(tx);

        // clearing the choice is refused (a rule of this addon: the column itself is not required)
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let user = create_user(&mut tx, "prefs_mail_clear", &[internal], None)
            .await
            .unwrap();
        let err = user
            .write_values(&mut tx, &one("notification_type", Value::False))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(err.message(), "The notification choice cannot be empty.");
        tx.rollback().await.unwrap();
        drop(tx);

        // a share user: the default is accepted, the in-application value is refused on create
        // and on write
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let portal_user = create_user(&mut tx, "prefs_mail_portal", &[portal], None)
            .await
            .unwrap();
        assert_eq!(value_of(&mut tx, &portal_user).await, default);
        let err = portal_user
            .write_values(&mut tx, &one("notification_type", Value::str("inbox")))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(err.message(), SHARE_REFUSAL);
        tx.rollback().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let err = create_user(&mut tx, "prefs_mail_portal_inbox", &[portal], Some("inbox"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(err.message(), SHARE_REFUSAL);
        tx.rollback().await.unwrap();
        drop(tx);

        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let user = create_user(&mut tx, "prefs_mail_demoted", &[internal], Some("inbox"))
            .await
            .unwrap();
        let err = user
            .write_values(&mut tx, &only_group("group_ids", portal))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(err.message(), SHARE_REFUSAL);
        tx.rollback().await.unwrap();
        drop(tx);

        // a row that holds no value (inserted by raw SQL) counts as e-mail: it may become a share
        // user, and the choice is read as no value until written
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let raw: i64 = tx
            .query(
                "INSERT INTO res_users (name, login, active, share) \
                 VALUES ('raw', 'prefs_mail_raw', true, false) RETURNING id::int8",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.invalidate_all().await.unwrap();
        let user = tx
            .env()
            .records("res.users")
            .unwrap()
            .browse([raw])
            .unwrap();
        assert_eq!(value_of(&mut tx, &user).await, Value::False);
        user.write_values(&mut tx, &only_group("group_ids", portal))
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}

/// The RPC host of the installed server: only `en_US` is an active language.
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

/// The names of the `user_writeable` fields of `res.users` in a registry, sorted.
fn user_writeable_names(orm: &Orm) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = orm
        .registry()
        .model("res.users")
        .unwrap()
        .fields
        .iter()
        .filter(|field| field.user_writeable)
        .map(|field| field.name)
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn an_internal_user_saves_their_own_notification_choice_and_nothing_else() {
    big_stack(async move {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let init = system
            .initialize(InitOptions::new().with_skip_auto_install(true))
            .await
            .unwrap();
        // the self-write set of base, from the registry before mail is installed
        let base_set = user_writeable_names(&init.orm);
        let env = system.system_env(erp_orm::Context::new());
        let outcome = system
            .button_immediate_install(&env, &[ModuleName::new("mail").unwrap()])
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean());
        let installed = outcome.orm;
        let mut expected = base_set;
        expected.push("notification_type");
        expected.sort_unstable();
        assert_eq!(user_writeable_names(&installed), expected);

        {
            let mut audit_tx = installed
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
        ensure_administrator(&installed).await;
        let (me, other) = {
            let mut tx = installed.begin(SUPERUSER_ID).await.unwrap();
            let internal = base_id(&mut tx, "res.groups", "group_user").await;
            let me = create_user(&mut tx, "prefs_mail_me", &[internal], None)
                .await
                .unwrap()
                .ids()[0];
            let other = create_user(&mut tx, "prefs_mail_other", &[internal], None)
                .await
                .unwrap()
                .ids()[0];
            tx.commit().await.unwrap();
            (me, other)
        };

        let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
            .await
            .unwrap();
        let resolver = Resolver::for_registry(installed.registry()).unwrap();
        let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
        let orm = Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
            .generation_attachment(erp_orm::GenerationAttachment::new(
                Arc::clone(installed.registry()),
                installed
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
        let logged = auth
            .authenticate(LoginRequest::new(
                "prefs_mail_me",
                "prefs_mail_me-password",
                ClientAddr::InProcess("user-preferences-mail-test"),
            ))
            .await
            .unwrap();
        let mut begun = auth
            .begin_session(logged.session().secret(), CredentialOptions::new())
            .await
            .unwrap();
        let principal = begun.tx.env().principal().clone();
        begun.tx.rollback().await.unwrap();
        let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
        let call = async |path: &str, body: serde_json::Value| {
            let response = service
                .call(RpcCall::json2(
                    path,
                    serde_json::to_vec(&body).unwrap(),
                    Caller::Principal(principal.clone()),
                ))
                .await;
            let json: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
            (response.status(), json)
        };

        // the own choice: accepted and read back
        let (status, saved) = call(
            "/json/2/res.users/write",
            serde_json::json!({"ids": [me], "vals": {"notification_type": "inbox"}}),
        )
        .await;
        assert_eq!(status, 200, "{saved}");
        let (status, rows) = call(
            "/json/2/res.users/read",
            serde_json::json!({"ids": [me], "fields": ["notification_type"]}),
        )
        .await;
        assert_eq!(status, 200, "{rows}");
        assert_eq!(rows[0]["notification_type"], "inbox");
        // another user's row is refused (the text names record rules and is not asserted here)
        let (status, foreign) = call(
            "/json/2/res.users/write",
            serde_json::json!({"ids": [other], "vals": {"notification_type": "inbox"}}),
        )
        .await;
        assert_eq!(status, 403, "{foreign}");
        assert_eq!(foreign["name"], "erp.exceptions.AccessError");
        // a field that is not user-writeable is refused with the field-level text
        let (status, refused) = call(
            "/json/2/res.users/write",
            serde_json::json!({"ids": [me], "vals": {"login": "someone-else"}}),
        )
        .await;
        assert_eq!(status, 403, "{refused}");
        assert_eq!(refused["name"], "erp.exceptions.AccessError");
        assert_eq!(
            refused["message"],
            "You do not have enough rights to access the field \"login\" on User (res.users). \
             Please contact your system administrator.\n\nOperation: write"
        );
        // a value outside the two choices is the selection field's own refusal
        let (status, wrong) = call(
            "/json/2/res.users/write",
            serde_json::json!({"ids": [me], "vals": {"notification_type": "sms"}}),
        )
        .await;
        assert_eq!(status, 422, "{wrong}");
        assert_eq!(
            wrong["message"],
            "Wrong value for res.users.notification_type: 'sms'"
        );
        db.cleanup().await.unwrap();
    });
}
