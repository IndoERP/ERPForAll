#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::{path::Path, sync::Arc, thread};

use erp_fields::{Cell, RecId};
use erp_modules::{
    AddonIndex, ModuleName,
    system::{InitOptions, ModuleSystem},
};
use erp_orm::{Command, Orm, RawVals, SUPERUSER_ID};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_sdk::views::{LimitKind, ViewLimits};
use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

#[path = "support/panel_contract_custom.rs"]
mod panel_contract_custom;

struct Host;
impl RpcHost for Host {
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

async fn assert_h18_generation_validator_is_used(
    orm: &Orm,
    db: &TestDb,
    admin: erp_orm::Principal,
    list_view: i64,
) {
    use erp_sdk::views::{
        CheckedViewInput, DescriptionBudget, DescriptionValue, DescriptionValueBuilder,
        InstalledClosure, MetadataFieldSet, OwnerBoundViewTypes, PublishedViewTypes, ViewError,
        ViewResult, ViewTypeContribution, ViewTypeDecl, ViewTypeDeclFn,
    };
    fn rejecting_list() -> ViewResult<ViewTypeDecl> {
        fn validate(input: &CheckedViewInput<'_>, _: &DescriptionBudget) -> ViewResult<()> {
            Err(ViewError::InvalidDescription(
                if input.mobile() {
                    "H18 mobile validator reached."
                } else {
                    "H18 test validator reached."
                }
                .to_owned(),
            ))
        }
        fn build(
            _: &CheckedViewInput<'_>,
            _: DescriptionValueBuilder<'_>,
        ) -> ViewResult<DescriptionValue> {
            Err(ViewError::InvalidDescription(
                "H18 builder must not run after refused validation.".to_owned(),
            ))
        }
        ViewTypeDecl::builder("web", "list")
            .wire_major(1)
            .renderer("list")
            .schema(&erp_sdk::views::list_schema()?)
            .validator(validate)
            .builder(build)
            .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
            .methods(&[])
            .finish()
    }
    fn form() -> ViewResult<ViewTypeDecl> {
        (erp_addon_web::FORM_VIEW_TYPE)()
    }
    fn search() -> ViewResult<ViewTypeDecl> {
        (erp_addon_web::SEARCH_VIEW_TYPE)()
    }
    static TYPES: [ViewTypeDeclFn; 3] = [rejecting_list, form, search];
    let registry = Arc::clone(orm.registry());
    let owner = *registry
        .addons()
        .iter()
        .find(|addon| addon.name == "web")
        .unwrap();
    let bound = OwnerBoundViewTypes::bind(
        Arc::clone(&registry),
        &[ViewTypeContribution::new(owner, &TYPES)],
    )
    .unwrap();
    let pairs: Vec<_> = registry
        .addons()
        .iter()
        .map(|addon| (addon.name, addon.depends))
        .collect();
    let table =
        PublishedViewTypes::publish(bound, InstalledClosure::new(&pairs).unwrap(), |_, _| false)
            .unwrap();
    let policy = PolicySource::build(Arc::clone(&registry), db.pool().clone())
        .await
        .unwrap();
    let resolver = Resolver::for_registry(&registry).unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(policy);
    let instrumented = Orm::builder(Arc::clone(&registry), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            registry,
            Arc::new(table),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(resolver)
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let service = RpcService::new(instrumented, Host, RpcConfig::default());
    for (options, expected) in [
        (
            Some(serde_json::json!({"mobile": false})),
            "H18 test validator reached.",
        ),
        (
            Some(serde_json::json!({"mobile": true})),
            "H18 mobile validator reached.",
        ),
        (None, "H18 test validator reached."),
    ] {
        let mut body = serde_json::json!({"views":[[list_view,"list"]],"wire_major":1});
        if let Some(options) = options {
            body["options"] = options;
        }
        let response = service
            .call(RpcCall::json2(
                "/json/2/acceptance_notes.note/get_views",
                serde_json::to_vec(&body).unwrap(),
                Caller::Principal(admin.clone()),
            ))
            .await;
        assert_eq!(
            response.status(),
            422,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let error = result(&response);
        assert_eq!(error["name"], "erp.exceptions.ValidationError");
        assert_eq!(error["message"], expected);
    }
}

fn capture_h18_description(name: &str, body: &serde_json::Value) {
    if let Ok(path) = std::env::var("ERP_H18_CAPTURE_DIR") {
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            std::path::Path::new(&path).join(name),
            serde_json::to_vec_pretty(body).unwrap(),
        )
        .unwrap();
    }
}

async fn fresh_metadata_policy_orm(orm: &Orm, db: &TestDb) -> Orm {
    let registry = Arc::clone(orm.registry());
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(
        PolicySource::build(Arc::clone(&registry), db.pool().clone())
            .await
            .unwrap(),
    );
    Orm::builder(Arc::clone(&registry), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(&registry),
            orm.env(SUPERUSER_ID, erp_orm::Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(&registry).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}

async fn assert_readonly_access_floors(
    orm: &Orm,
    db: &TestDb,
    _service: &RpcService,
    ordinary: erp_orm::Principal,
    form_view: i64,
) {
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let original: String = setup
        .query("SELECT arch_db FROM ir_ui_view WHERE id=$1", &[&form_view])
        .await
        .unwrap()[0]
        .get(0);
    let users=setup.env().records("ir.ui.view").unwrap().create_values(&mut setup,&[[
        ("name".to_owned(),erp_sdk::Value::str("H18 user readonly fixture")),
        ("model".to_owned(),erp_sdk::Value::str("res.users")),
        ("arch_db".to_owned(),erp_sdk::Value::str("<form><field name='login' readonly='0'/><field name='login' readonly='1'/><field name='email' readonly='0'/></form>")),
    ].into_iter().collect()]).await.unwrap().ids()[0];
    let user_group = relation_id(&mut setup, "group_user", "res.groups").await;
    let user_model: i64 = setup
        .query("SELECT id FROM ir_model WHERE model='res.users'", &[])
        .await
        .unwrap()[0]
        .get(0);
    let user_grant = setup
        .env()
        .records("ir.access")
        .unwrap()
        .create_values(
            &mut setup,
            &[[
                (
                    "name".to_owned(),
                    erp_sdk::Value::str("H18 field-mask fixture model Write grant"),
                ),
                ("model_id".to_owned(), erp_sdk::Value::Int(user_model)),
                ("group_id".to_owned(), erp_sdk::Value::Int(user_group)),
                ("operation".to_owned(), erp_sdk::Value::str("ru")),
            ]
            .into_iter()
            .collect()],
        )
        .await
        .unwrap()
        .ids()[0];
    setup.commit().await.unwrap();
    drop(setup);
    let user_orm = fresh_metadata_policy_orm(orm, db).await;
    let user_service = RpcService::new(user_orm.clone(), Host, RpcConfig::default());
    // The actual base user field-access chain permits the model's Write but denies login Write.
    let mut tx = user_orm.begin(ordinary.subject()).await.unwrap();
    let records = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([ordinary.subject().get()])
        .unwrap();
    records
        .check_access(&mut tx, erp_orm::Operation::Write)
        .await
        .unwrap();
    assert!(
        !orm.registry()
            .model("res.users")
            .unwrap()
            .field("login")
            .unwrap()
            .readonly
    );
    let metadata = records
        .fields_get(&mut tx, Some(&["login", "email"]), Some(&["readonly"]))
        .await
        .unwrap();
    assert_eq!(metadata["login"]["readonly"], erp_sdk::Value::Bool(true));
    assert_eq!(metadata["email"]["readonly"], erp_sdk::Value::Bool(false));
    let denied_values: erp_sdk::ValueMap = [(
        "login".to_owned(),
        erp_sdk::Value::str("forbidden-login-change"),
    )]
    .into_iter()
    .collect();
    let error = records
        .write_values(&mut tx, &denied_values)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"login\" on User (res.users). Please contact your system administrator.\n\nOperation: write"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let response = user_service
        .call(RpcCall::json2(
            "/json/2/res.users/get_views",
            serde_json::to_vec(&serde_json::json!({"views":[[users,"form"]],"wire_major":1}))
                .unwrap(),
            Caller::Principal(ordinary.clone()),
        ))
        .await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body = result(&response);
    assert_eq!(body["fields"]["res.users"]["login"]["readonly"], true);
    assert_eq!(body["fields"]["res.users"]["email"]["readonly"], false);
    assert_eq!(
        body["views"][0]["nodes"][1]["modifiers"]["readonly"]["nodes"][0]["value"]["value"],
        false
    );
    capture_h18_description("readonly-field-host.json", &body);
    // Model Write denial is produced by actual compiled ir.access rows, not a synthetic mask.
    let mut alter = orm.begin(SUPERUSER_ID).await.unwrap();
    let note = alter
        .env()
        .records("acceptance_notes.note")
        .unwrap()
        .create_values(
            &mut alter,
            &[[
                (
                    "name".to_owned(),
                    erp_sdk::Value::str("H18 readonly owned note"),
                ),
                (
                    "company_id".to_owned(),
                    erp_sdk::Value::Int(ordinary.companies().current().unwrap().get()),
                ),
                (
                    "owner_id".to_owned(),
                    erp_sdk::Value::Int(ordinary.subject().get()),
                ),
            ]
            .into_iter()
            .collect()],
        )
        .await
        .unwrap()
        .ids()[0];
    let rows=alter.query("SELECT id,operation FROM ir_access WHERE model_id=(SELECT id FROM ir_model WHERE model=$1) AND group_id IS NOT NULL",&[&"acceptance_notes.note"]).await.unwrap();
    let permissions: Vec<(i64, String)> = rows.iter().map(|row| (row.get(0), row.get(1))).collect();
    assert!(!permissions.is_empty());
    alter.execute("UPDATE ir_access SET operation='r' WHERE model_id=(SELECT id FROM ir_model WHERE model=$1) AND group_id IS NOT NULL",&[&"acceptance_notes.note"]).await.unwrap();
    alter.commit().await.unwrap();
    drop(alter);
    let registry = Arc::clone(orm.registry());
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(
        PolicySource::build(Arc::clone(&registry), db.pool().clone())
            .await
            .unwrap(),
    );
    let readonly = Orm::builder(Arc::clone(&registry), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(&registry),
            orm.env(SUPERUSER_ID, erp_orm::Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(&registry).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let mut tx = readonly.begin(ordinary.subject()).await.unwrap();
    let records = tx
        .env()
        .records("acceptance_notes.note")
        .unwrap()
        .browse([note])
        .unwrap();
    let error = records
        .check_access(&mut tx, erp_orm::Operation::Write)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you modify 'Note' (acceptance_notes.note) records.\n\nNo group may do this.\n\nAsk an administrator if you need this access."
    );
    let values: erp_sdk::ValueMap = [(
        "name".to_owned(),
        erp_sdk::Value::str("Forbidden model write"),
    )]
    .into_iter()
    .collect();
    let denied = records.write_values(&mut tx, &values).await.unwrap_err();
    assert_eq!(denied.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(denied.message(), error.message());
    let unchanged: String = tx
        .query(
            "SELECT name FROM acceptance_notes_note WHERE id=$1",
            &[&note],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(unchanged, "H18 readonly owned note");
    let metadata = records
        .fields_get(&mut tx, Some(&["name"]), Some(&["readonly"]))
        .await
        .unwrap();
    assert_eq!(metadata["name"]["readonly"], erp_sdk::Value::Bool(true));
    tx.rollback().await.unwrap();
    drop(tx);
    let response = stored_form(
        &readonly,
        &RpcService::new(readonly.clone(), Host, RpcConfig::default()),
        ordinary.clone(),
        form_view,
        "<form><field name='name' readonly='0'/><field name='name' readonly='1'/></form>",
    )
    .await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body = result(&response);
    assert_eq!(
        body["fields"]["acceptance_notes.note"]["name"]["readonly"],
        true
    );
    assert_eq!(
        body["views"][0]["nodes"][1]["modifiers"]["readonly"]["nodes"][0]["value"]["value"],
        false
    );
    capture_h18_description("readonly-model-host.json", &body);
    let mut restore = orm.begin(SUPERUSER_ID).await.unwrap();
    restore
        .env()
        .records("ir.access")
        .unwrap()
        .browse([user_grant])
        .unwrap()
        .unlink(&mut restore)
        .await
        .unwrap();
    for (id, operation) in permissions {
        restore
            .execute(
                "UPDATE ir_access SET operation=$1 WHERE id=$2",
                &[&operation, &id],
            )
            .await
            .unwrap();
    }
    restore
        .env()
        .records("acceptance_notes.note")
        .unwrap()
        .browse([note])
        .unwrap()
        .unlink(&mut restore)
        .await
        .unwrap();
    restore
        .env()
        .records("ir.ui.view")
        .unwrap()
        .browse([users])
        .unwrap()
        .unlink(&mut restore)
        .await
        .unwrap();
    restore.commit().await.unwrap();
    drop(restore);
    write_test_arch(orm, form_view, &original).await;
}

async fn custom_search_operator_response(
    orm: &Orm,
    db: &TestDb,
    principal: erp_orm::Principal,
    view: i64,
    foreign: bool,
) -> erp_rpc::RpcResponse {
    use erp_sdk::views::*;
    fn search<const FOREIGN: bool>() -> ViewResult<ViewTypeDecl> {
        fn validate(_: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
            budget.check()
        }
        fn build<const FOREIGN: bool>(
            input: &CheckedViewInput<'_>,
            root: DescriptionValueBuilder<'_>,
        ) -> ViewResult<DescriptionValue> {
            root.object(|object| {
                object.set("root", |value| value.integer(0))?;
                object.set("nodes", |value| {
                    value.array(2, |index, value| {
                        if index == 0 {
                            value.variant("search", |value| {
                                value.object(|object| {
                                    object.set("children", |value| {
                                        value.array(1, |_, value| value.integer(1))
                                    })
                                })
                            })
                        } else {
                            value.variant("search_field", |value| {
                                value.object(|object| {
                                    object.set("field", |value| {
                                        value.field_handle(
                                            if FOREIGN {
                                                "res.company"
                                            } else {
                                                input.root_model()
                                            },
                                            "name",
                                        )
                                    })?;
                                    object.set("label", |value| value.text("Name"))?;
                                    object.set("operator", |value| {
                                        value.text(if FOREIGN {
                                            "ilike"
                                        } else {
                                            "unknown_operator"
                                        })
                                    })?;
                                    Ok(())
                                })
                            })
                        }
                    })
                })?;
                Ok(())
            })
        }
        ViewTypeDecl::builder("web", "search")
            .wire_major(1)
            .renderer("web.search")
            .schema(&search_schema()?)
            .validator(validate)
            .builder(build::<FOREIGN>)
            .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
            .methods(&[])
            .finish()
    }
    fn list() -> ViewResult<ViewTypeDecl> {
        (erp_addon_web::LIST_VIEW_TYPE)()
    }
    fn form() -> ViewResult<ViewTypeDecl> {
        (erp_addon_web::FORM_VIEW_TYPE)()
    }
    static TYPES: [ViewTypeDeclFn; 3] = [list, form, search::<false>];
    static FOREIGN_TYPES: [ViewTypeDeclFn; 3] = [list, form, search::<true>];
    get_views_with_types(
        orm,
        db,
        principal,
        view,
        "search",
        if foreign { &FOREIGN_TYPES } else { &TYPES },
    )
    .await
}

async fn assert_button_context_caller_admission(
    orm: &Orm,
    db: &TestDb,
    admin: erp_orm::Principal,
    ordinary: erp_orm::Principal,
    view: i64,
) {
    use erp_sdk::views::*;
    fn form() -> ViewResult<ViewTypeDecl> {
        fn validate(_: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
            budget.check()
        }
        fn build(
            _: &CheckedViewInput<'_>,
            root: DescriptionValueBuilder<'_>,
        ) -> ViewResult<DescriptionValue> {
            let ast = erp_sdk::expr::wire::ValidatedAst::from_expr(
                &erp_sdk::expr::parse("{'default_body': internal_memo}").unwrap(),
            )
            .unwrap();
            root.object(|o| {
                o.set("root", |v| v.integer(0))?;
                o.set("nodes", |v| {
                    v.array(3, |index, v| match index {
                        0 | 1 => v.variant(if index == 0 { "form" } else { "header" }, |v| {
                            v.object(|o| {
                                if index == 0 {
                                    o.set("record_mode", |v| v.text("record"))?;
                                }
                                o.set("children", |v| {
                                    v.array(1, |_, v| v.integer(index as i64 + 1))
                                })
                            })
                        }),
                        _ => v.variant("action_button", |v| {
                            v.object(|o| {
                                o.set("target", |v| v.variant("inert", |v| v.object(|_| Ok(()))))?;
                                o.set("label", |v| v.text("Context only"))?;
                                o.set("variant", |v| v.text("secondary"))?;
                                o.set("modifiers", |v| v.object(|_| Ok(())))?;
                                o.set("details", |v| {
                                    v.object(|d| {
                                        d.set("size", |v| v.text("normal"))?;
                                        d.set("circle", |v| v.boolean(false))?;
                                        d.set("classes", |v| v.array(0, |_, v| v.text("unused")))?;
                                        d.set("debounce_ms", |v| v.integer(0))?;
                                        d.set("block_ui", |v| v.boolean(false))?;
                                        d.set("context", |v| v.expression(&ast))
                                    })
                                })
                            })
                        }),
                    })
                })
            })
        }
        ViewTypeDecl::builder("web", "form")
            .wire_major(1)
            .renderer("web.form")
            .schema(&form_schema()?)
            .validator(validate)
            .builder(build)
            .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
            .methods(&[])
            .finish()
    }
    fn list() -> ViewResult<ViewTypeDecl> {
        (erp_addon_web::LIST_VIEW_TYPE)()
    }
    fn search() -> ViewResult<ViewTypeDecl> {
        (erp_addon_web::SEARCH_VIEW_TYPE)()
    }
    static TYPES: [ViewTypeDeclFn; 3] = [list, form, search];
    let denied = get_views_with_types(orm, db, ordinary, view, "form", &TYPES).await;
    assert_eq!(
        denied.status(),
        403,
        "{}",
        String::from_utf8_lossy(denied.body())
    );
    assert_eq!(result(&denied)["name"], "erp.exceptions.AccessError");
    assert_eq!(
        result(&denied)["message"],
        "A nonprunable builtin node references a field the caller cannot read."
    );
    let allowed = get_views_with_types(orm, db, admin, view, "form", &TYPES).await;
    assert_eq!(
        allowed.status(),
        200,
        "{}",
        String::from_utf8_lossy(allowed.body())
    );
    let body = result(&allowed);
    assert_eq!(
        body["views"][0]["nodes"][2]["target"],
        serde_json::json!({"kind":"inert"})
    );
    assert_eq!(
        body["views"][0]["nodes"][2]["details"]["context"]["schema"],
        1
    );
    assert!(body["fields"]["acceptance_notes.note"]["internal_memo"].is_object());
}

async fn assert_served_wire_attributes(
    orm: &Orm,
    service: &RpcService,
    principal: erp_orm::Principal,
    view: i64,
    action: i64,
) {
    let arch = format!(
        r#"<form><header>
        <button name="action_archive" type="object" string="" title="Archive record" help="Keep the record for reference."
          icon="lucide:refresh-cw" size="sm" class="btn-success rounded-circle addon_archive" data-hotkey="shift+a"
          debounce="300" block-ui="1" confirm="Archive this record?" confirm-title="Review archive"
          confirm-label="Continue" cancel-label="Continue" context="{{'default_priority': priority}}"/>
        <button name="{action}" type="action" string="Open notes"/>
        <button type="url" href="/documentation" string="Documentation" class="btn-link"/>
        <button string="Unavailable operation"/>
        </header><sheet><widget name="web_ribbon" title="Ready" text="Fallback" tooltip="Record status"/>
        <widget name="notification_alert"/><field name="name"/><field name="priority"/></sheet>
        <footer><button special="cancel" string="" title="Close dialog" icon="lucide:refresh-cw"/></footer></form>"#
    );
    let stored = stored_form(orm, service, principal.clone(), view, &arch).await;
    assert_eq!(
        stored.status(),
        200,
        "{}",
        String::from_utf8_lossy(stored.body())
    );
    // An installed extension changes the new details on the actual combined H18 arch.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let extension = tx.env().records("ir.ui.view").unwrap().create(&mut tx, vec![RawVals::new()
        .set_cell("name", Cell::Str("metadata.wire.attributes.extension".into()))
        .set_cell("model", Cell::Str("acceptance_notes.note".into()))
        .set_cell("inherit_id", Cell::Id(view))
        .set_cell("arch_db", Cell::Str(r#"<xpath expr="//button[@name='action_archive']" position="attributes"><attribute name="help">Keep the inherited record for reference.</attribute><attribute name="context">{'default_priority': priority, 'default_body': body}</attribute></xpath>"#.into()))
    ]).await.unwrap().ids()[0];
    tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    tx.commit().await.unwrap();
    drop(tx);
    // Capture the real list+form response; the browser must not synthesize another view.
    let response = service
        .call(RpcCall::json2(
            "/json/2/acceptance_notes.note/get_views",
            serde_json::to_vec(
                &serde_json::json!({"views":[[null,"list"],[view,"form"]],"wire_major":1}),
            )
            .unwrap(),
            Caller::Principal(principal),
        ))
        .await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body = result(&response);
    let views = body["views"].as_array().unwrap();
    assert_eq!(views.len(), 2);
    assert!(views.iter().any(|view| view["nodes"][0]["kind"] == "list"));
    let form = views
        .iter()
        .find(|view| view["nodes"][0]["kind"] == "form")
        .unwrap();
    let nodes = form["nodes"].as_array().unwrap();
    let archive = nodes
        .iter()
        .find(|node| node["kind"] == "button" && node["name"] == "action_archive")
        .unwrap();
    assert_eq!(archive["label"], "");
    assert_eq!(archive["variant"], "success");
    assert_eq!(archive["confirm"], "Archive this record?");
    let details = archive["details"].as_object().unwrap();
    for (key, expected) in [
        ("size", serde_json::json!("small")),
        ("circle", serde_json::json!(true)),
        ("classes", serde_json::json!(["addon_archive"])),
        ("icon", serde_json::json!("lucide:refresh-cw")),
        ("title", serde_json::json!("Archive record")),
        (
            "help",
            serde_json::json!("Keep the inherited record for reference."),
        ),
        ("hotkey", serde_json::json!("shift+a")),
        ("debounce_ms", serde_json::json!(300)),
        ("block_ui", serde_json::json!(true)),
        ("confirm_title", serde_json::json!("Review archive")),
        ("confirm_label", serde_json::json!("Continue")),
        ("cancel_label", serde_json::json!("Continue")),
    ] {
        assert_eq!(details.get(key), Some(&expected), "{key}");
    }
    assert_eq!(details.len(), 13);
    let ast = erp_sdk::expr::wire::ValidatedAst::from_expr(
        &erp_sdk::expr::parse("{'default_priority': priority, 'default_body': body}").unwrap(),
    )
    .unwrap();
    let context: serde_json::Value = serde_json::from_slice(&ast.to_json().unwrap()).unwrap();
    assert_eq!(details.get("context"), Some(&context));
    for target in [
        serde_json::json!({"kind":"action","id":action}),
        serde_json::json!({"kind":"url","href":"/documentation"}),
        serde_json::json!({"kind":"inert"}),
    ] {
        let node = nodes
            .iter()
            .find(|node| node["kind"] == "action_button" && node["target"] == target)
            .unwrap();
        assert!(node.get("name").is_none());
        assert!(node.get("button_type").is_none());
    }
    assert!(nodes.iter().any(|node| *node == serde_json::json!({"kind":"ribbon","text":"Ready","tooltip":"Record status","background":"success"})));
    assert!(
        nodes
            .iter()
            .any(|node| *node == serde_json::json!({"kind":"notification_alert"}))
    );
    assert!(nodes.iter().any(|node| node["kind"] == "cancel_button"
        && node["label"] == ""
        && node["details"]["title"] == "Close dialog"));
    assert!(body["fields"]["acceptance_notes.note"]["priority"].is_object());
    assert!(body["fields"]["acceptance_notes.note"]["body"].is_object());
    assert!(body["fields"]["acceptance_notes.note"]["internal_memo"].is_null());
    capture_h18_description("wire-attrs-host.json", &body);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("DELETE FROM ir_ui_view WHERE id = $1", &[&extension])
        .await
        .unwrap();
    tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    tx.commit().await.unwrap();
}

/// Serve `get_views` for one stored view through a separately published custom type table.
async fn get_views_with_types(
    orm: &Orm,
    db: &TestDb,
    principal: erp_orm::Principal,
    view: i64,
    view_type: &str,
    types: &'static [erp_sdk::views::ViewTypeDeclFn],
) -> erp_rpc::RpcResponse {
    use erp_sdk::views::*;
    let registry = Arc::clone(orm.registry());
    let owner = *registry
        .addons()
        .iter()
        .find(|addon| addon.name == "web")
        .unwrap();
    let bound = OwnerBoundViewTypes::bind(
        Arc::clone(&registry),
        &[ViewTypeContribution::new(owner, types)],
    )
    .unwrap();
    let pairs: Vec<_> = registry
        .addons()
        .iter()
        .map(|addon| (addon.name, addon.depends))
        .collect();
    let table =
        PublishedViewTypes::publish(bound, InstalledClosure::new(&pairs).unwrap(), |_, _| false)
            .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(
        PolicySource::build(Arc::clone(&registry), db.pool().clone())
            .await
            .unwrap(),
    );
    let resolver = Resolver::for_registry(&registry).unwrap();
    let instrumented = Orm::builder(Arc::clone(&registry), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            registry,
            Arc::new(table),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(resolver)
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    RpcService::new(instrumented, Host, RpcConfig::default())
        .call(RpcCall::json2(
            "/json/2/acceptance_notes.note/get_views",
            serde_json::to_vec(&serde_json::json!({"views":[[view,view_type]],"wire_major":1}))
                .unwrap(),
            Caller::Principal(principal),
        ))
        .await
}

const SLOT_LIST_FIELD: u8 = 0;
const SLOT_SEARCH_FIELD: u8 = 1;
const SLOT_SELECTION_PANEL: u8 = 2;

/// Custom builders for the three builtin field-handle slots. `DOTTED` swaps the direct root
/// name for a same-model relation path (`company_id.name`) that the client catalogue cannot
/// resolve as a direct field.
fn handle_types<const SLOT: u8, const DOTTED: bool>() -> [erp_sdk::views::ViewTypeDeclFn; 3] {
    use erp_sdk::views::*;
    fn list<const SLOT: u8, const DOTTED: bool>() -> ViewResult<ViewTypeDecl> {
        fn validate(_: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
            budget.check()
        }
        fn build<const DOTTED: bool>(
            input: &CheckedViewInput<'_>,
            root: DescriptionValueBuilder<'_>,
        ) -> ViewResult<DescriptionValue> {
            root.object(|object| {
                object.set("root", |value| value.integer(0))?;
                object.set("nodes", |value| {
                    value.array(2, |index, value| {
                        if index == 0 {
                            value.variant("list", |value| {
                                value.object(|object| {
                                    object.set("children", |value| {
                                        value.array(1, |_, value| value.integer(1))
                                    })
                                })
                            })
                        } else {
                            value.variant("field", |value| {
                                value.object(|object| {
                                    object.set("field", |value| {
                                        value.field_handle(
                                            input.root_model(),
                                            if DOTTED { "company_id.name" } else { "name" },
                                        )
                                    })?;
                                    object.set("widget", |value| value.text("char"))?;
                                    object.set("options", |value| value.object(|_| Ok(())))?;
                                    object.set("modifiers", |value| value.object(|_| Ok(())))?;
                                    object.set("label", |value| value.text("Name"))?;
                                    object.set("force_save", |value| value.boolean(false))?;
                                    Ok(())
                                })
                            })
                        }
                    })
                })
            })
        }
        ViewTypeDecl::builder("web", "list")
            .wire_major(1)
            .renderer("web.list")
            .schema(&list_schema()?)
            .validator(validate)
            .builder(build::<DOTTED>)
            .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
            .methods(&[])
            .finish()
    }
    fn search<const SLOT: u8, const DOTTED: bool>() -> ViewResult<ViewTypeDecl> {
        fn validate(_: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
            budget.check()
        }
        fn build<const SLOT: u8, const DOTTED: bool>(
            input: &CheckedViewInput<'_>,
            root: DescriptionValueBuilder<'_>,
        ) -> ViewResult<DescriptionValue> {
            let panel = SLOT == SLOT_SELECTION_PANEL;
            root.object(|object| {
                object.set("root", |value| value.integer(0))?;
                object.set("nodes", |value| {
                    value.array(if panel { 1 } else { 2 }, |index, value| {
                        if index == 0 {
                            value.variant("search", |value| {
                                value.object(|object| {
                                    object.set("children", |value| {
                                        value
                                            .array(usize::from(!panel), |_, value| value.integer(1))
                                    })?;
                                    if panel {
                                        object.set("search_panel", |value| {
                                            value.variant("selection", |value| {
                                                value.object(|object| {
                                                    object.set("field", |value| {
                                                        value.field_handle(
                                                            input.root_model(),
                                                            if DOTTED {
                                                                "company_id.name"
                                                            } else {
                                                                "priority"
                                                            },
                                                        )
                                                    })?;
                                                    object.set("label", |value| {
                                                        value.text("Priority")
                                                    })?;
                                                    object.set("enable_counters", |value| {
                                                        value.boolean(false)
                                                    })?;
                                                    object.set("expand", |value| {
                                                        value.boolean(false)
                                                    })?;
                                                    Ok(())
                                                })
                                            })
                                        })?;
                                    }
                                    Ok(())
                                })
                            })
                        } else {
                            value.variant("search_field", |value| {
                                value.object(|object| {
                                    object.set("field", |value| {
                                        value.field_handle(
                                            input.root_model(),
                                            if DOTTED { "company_id.name" } else { "name" },
                                        )
                                    })?;
                                    object.set("label", |value| value.text("Name"))?;
                                    object.set("operator", |value| value.text("ilike"))?;
                                    Ok(())
                                })
                            })
                        }
                    })
                })
            })
        }
        ViewTypeDecl::builder("web", "search")
            .wire_major(1)
            .renderer("web.search")
            .schema(&search_schema()?)
            .validator(validate)
            .builder(build::<SLOT, DOTTED>)
            .metadata_fields(MetadataFieldSet::builder().arch_references().finish()?)
            .methods(&[])
            .finish()
    }
    fn form() -> ViewResult<ViewTypeDecl> {
        (erp_addon_web::FORM_VIEW_TYPE)()
    }
    [list::<SLOT, DOTTED>, form, search::<SLOT, DOTTED>]
}

async fn assert_builtin_handles_name_direct_root_fields(
    orm: &Orm,
    db: &TestDb,
    ordinary: erp_orm::Principal,
    list_view: i64,
    search_view: i64,
) {
    static LIST_OK: std::sync::OnceLock<[erp_sdk::views::ViewTypeDeclFn; 3]> =
        std::sync::OnceLock::new();
    static LIST_DOTTED: std::sync::OnceLock<[erp_sdk::views::ViewTypeDeclFn; 3]> =
        std::sync::OnceLock::new();
    static SEARCH_OK: std::sync::OnceLock<[erp_sdk::views::ViewTypeDeclFn; 3]> =
        std::sync::OnceLock::new();
    static SEARCH_DOTTED: std::sync::OnceLock<[erp_sdk::views::ViewTypeDeclFn; 3]> =
        std::sync::OnceLock::new();
    static PANEL_OK: std::sync::OnceLock<[erp_sdk::views::ViewTypeDeclFn; 3]> =
        std::sync::OnceLock::new();
    static PANEL_DOTTED: std::sync::OnceLock<[erp_sdk::views::ViewTypeDeclFn; 3]> =
        std::sync::OnceLock::new();
    const LIST: u8 = SLOT_LIST_FIELD;
    const SEARCH: u8 = SLOT_SEARCH_FIELD;
    const PANEL: u8 = SLOT_SELECTION_PANEL;
    let cases: [(
        &str,
        i64,
        &str,
        &'static [erp_sdk::views::ViewTypeDeclFn],
        bool,
    ); 6] = [
        (
            "field",
            list_view,
            "list",
            LIST_OK.get_or_init(handle_types::<LIST, false>),
            false,
        ),
        (
            "field",
            list_view,
            "list",
            LIST_DOTTED.get_or_init(handle_types::<LIST, true>),
            true,
        ),
        (
            "search_field",
            search_view,
            "search",
            SEARCH_OK.get_or_init(handle_types::<SEARCH, false>),
            false,
        ),
        (
            "search_field",
            search_view,
            "search",
            SEARCH_DOTTED.get_or_init(handle_types::<SEARCH, true>),
            true,
        ),
        (
            "search_panel",
            search_view,
            "search",
            PANEL_OK.get_or_init(handle_types::<PANEL, false>),
            false,
        ),
        (
            "search_panel",
            search_view,
            "search",
            PANEL_DOTTED.get_or_init(handle_types::<PANEL, true>),
            true,
        ),
    ];
    for (kind, view, view_type, types, dotted) in cases {
        let response =
            get_views_with_types(orm, db, ordinary.clone(), view, view_type, types).await;
        eprintln!(
            "builtin_handle kind={kind} dotted={dotted} status={} body={}",
            response.status(),
            String::from_utf8_lossy(response.body())
        );
        if dotted {
            assert_eq!(response.status(), 422, "{kind}");
            let error = result(&response);
            assert_eq!(error["name"], "erp.exceptions.ValidationError", "{kind}");
            assert_eq!(
                error["message"],
                format!(
                    "Builtin {kind} reference 'acceptance_notes.note.company_id.name' must name a direct field of root model 'acceptance_notes.note'. Relation paths in builtin field handles require a later release."
                ),
                "{kind}"
            );
        } else {
            assert_eq!(
                response.status(),
                200,
                "{kind}: {}",
                String::from_utf8_lossy(response.body())
            );
        }
    }
}

async fn assert_search_field_operator_contract(
    orm: &Orm,
    db: &TestDb,
    service: &RpcService,
    ordinary: erp_orm::Principal,
    view: i64,
) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let original: String = tx
        .query("SELECT arch_db FROM ir_ui_view WHERE id=$1", &[&view])
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    for operator in [
        None,
        Some("ilike"),
        Some("="),
        Some("!="),
        Some(">"),
        Some("<"),
        Some(">="),
        Some("<="),
    ] {
        let attribute = operator
            .map(|op| {
                format!(
                    " operator=\"{}\"",
                    op.replace('<', "&lt;").replace('>', "&gt;")
                )
            })
            .unwrap_or_default();
        write_test_arch(
            orm,
            view,
            &format!("<search><field name=\"name\"{attribute}/></search>"),
        )
        .await;
        let response = service
            .call(RpcCall::json2(
                "/json/2/acceptance_notes.note/get_views",
                serde_json::to_vec(&serde_json::json!({"views":[[view,"search"]],"wire_major":1}))
                    .unwrap(),
                Caller::Principal(ordinary.clone()),
            ))
            .await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let body = result(&response);
        assert_eq!(
            body["views"][0]["nodes"][1]["operator"],
            operator
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null)
        );
    }
    write_test_arch(orm,view,r#"<search><field name="name" operator="ilike" filter_domain="[('name','ilike',self)]"/><filter name="named" string="Named" domain="[('name','!=',False)]"/><separator/><group string="Group" expand="1"><filter name="priority" string="Priority" context="{'group_by':'priority'}"/><separator/></group><searchpanel><field name="priority" expand="1"/></searchpanel></search>"#).await;
    let actual = service
        .call(RpcCall::json2(
            "/json/2/acceptance_notes.note/get_views",
            serde_json::to_vec(&serde_json::json!({"views":[[view,"search"]],"wire_major":1}))
                .unwrap(),
            Caller::Principal(ordinary.clone()),
        ))
        .await;
    assert_eq!(
        actual.status(),
        200,
        "{}",
        String::from_utf8_lossy(actual.body())
    );
    capture_h18_description("search-host.json", &result(&actual));
    write_test_arch(
        orm,
        view,
        "<search><field name=\"name\" operator=\"unknown_operator\"/></search>",
    )
    .await;
    let response = service
        .call(RpcCall::json2(
            "/json/2/acceptance_notes.note/get_views",
            serde_json::to_vec(&serde_json::json!({"views":[[view,"search"]],"wire_major":1}))
                .unwrap(),
            Caller::Principal(ordinary.clone()),
        ))
        .await;
    let custom = custom_search_operator_response(orm, db, ordinary.clone(), view, false).await;
    eprintln!(
        "operator_xml_status={} operator_custom_status={} xml_body={} custom_body={}",
        response.status(),
        custom.status(),
        String::from_utf8_lossy(response.body()),
        String::from_utf8_lossy(custom.body())
    );
    assert_eq!(
        response.status(),
        422,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let error = result(&response);
    assert_eq!(error["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        error["message"],
        "Unsupported search field operator 'unknown_operator' for 'acceptance_notes.note.name'. Additional operators require a later release."
    );
    assert_eq!(custom.status(), 422);
    assert_eq!(result(&custom)["name"], "erp.exceptions.ValidationError");
    assert_eq!(result(&custom)["message"], error["message"]);
    let foreign = custom_search_operator_response(orm, db, ordinary, view, true).await;
    eprintln!(
        "foreign_builtin_status={} body={}",
        foreign.status(),
        String::from_utf8_lossy(foreign.body())
    );
    assert_eq!(foreign.status(), 422);
    assert_eq!(result(&foreign)["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        result(&foreign)["message"],
        "Builtin search_field reference 'res.company.name' does not match root model 'acceptance_notes.note'. Cross-model builtin fields require a later release."
    );
    write_test_arch(orm, view, &original).await;
}

/// One serving instance as the native server composes it: its own ORM and policy source on the
/// shared database, a generation baseline read at start and the policy source as refresh hook.
/// No listener runs, so no wake-up ever reaches it: only the request-opening generation read can
/// notice another process's commit (the missed-signal path).
async fn serving_instance(orm: &Orm, db: &TestDb) -> (Orm, RpcService) {
    let registry = Arc::clone(orm.registry());
    let source = PolicySource::build(Arc::clone(&registry), db.pool().clone())
        .await
        .unwrap();
    let baseline = {
        let client = db.pool().get().await.unwrap();
        erp_orm::Signal::read(&client).await.unwrap()
    };
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source.clone());
    let instance = Orm::builder(Arc::clone(&registry), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(&registry),
            orm.env(SUPERUSER_ID, erp_orm::Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(&registry).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .signal(erp_orm::Signal::new(baseline).with_refresh(source))
        .build()
        .unwrap();
    let service = RpcService::new(instance.clone(), Host, RpcConfig::default());
    (instance, service)
}

async fn database_generation(db: &TestDb, key: &erp_orm::SignalKey) -> u64 {
    let client = db.pool().get().await.unwrap();
    erp_orm::Signal::read(&client).await.unwrap().get(key)
}

/// Field labels of the served form and the served readonly flag of `name`.
async fn served_form_probe(
    service: &RpcService,
    principal: &erp_orm::Principal,
    view: i64,
) -> (Vec<String>, Option<bool>) {
    let response = service
        .call(RpcCall::json2(
            "/json/2/acceptance_notes.note/get_views",
            serde_json::to_vec(&serde_json::json!({"views":[[view,"form"]],"wire_major":1}))
                .unwrap(),
            Caller::Principal(principal.clone()),
        ))
        .await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body = result(&response);
    let labels = body["views"][0]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["kind"] == "field")
        .map(|node| node["label"].as_str().unwrap().to_owned())
        .collect();
    let readonly = body["fields"]["acceptance_notes.note"]["name"]["readonly"].as_bool();
    (labels, readonly)
}

async fn served_menu_ids(
    service: &RpcService,
    principal: &erp_orm::Principal,
) -> std::collections::BTreeSet<i64> {
    let response = service
        .call(RpcCall::json2(
            "/json/2/ir.ui.menu/erp_load_web_menus",
            br#"{}"#.to_vec(),
            Caller::Principal(principal.clone()),
        ))
        .await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    result(&response)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_i64().unwrap())
        .collect()
}

/// Plan acceptance 8: two independent serving instances on one database see committed view,
/// access-policy and menu changes on their next admitted request, with no wake-up delivered
/// (missed-signal recovery). Functional proof only; no throughput claim.
async fn assert_two_serving_instances_share_database(
    orm: &Orm,
    db: &TestDb,
    ordinary: erp_orm::Principal,
    form_view: i64,
) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.install_signal().await.unwrap();
    let original: String = tx
        .query("SELECT arch_db FROM ir_ui_view WHERE id=$1", &[&form_view])
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();
    drop(tx);
    let (a, a_service) = serving_instance(orm, db).await;
    let (b, b_service) = serving_instance(orm, db).await;
    let policy = erp_orm::SignalKey::Policy;

    // Stored view change, committed by instance A (named-cache invalidation), seen by both.
    write_test_arch(
        &a,
        form_view,
        "<form><field name='name' string='Alpha'/></form>",
    )
    .await;
    for service in [&a_service, &b_service] {
        let (labels, readonly) = served_form_probe(service, &ordinary, form_view).await;
        assert_eq!(labels, ["Alpha"]);
        assert_ne!(readonly, Some(true));
    }
    write_test_arch(
        &a,
        form_view,
        "<form><field name='name' string='Beta'/></form>",
    )
    .await;
    for service in [&a_service, &b_service] {
        assert_eq!(
            served_form_probe(service, &ordinary, form_view).await.0,
            ["Beta"]
        );
    }

    // Access-policy change committed by a third process: neither instance is told.
    let applied_before = [&a, &b].map(|i| i.signal().unwrap().applied().get(&policy));
    let applies_before = [&a, &b].map(|i| i.signal().unwrap().stats().applies);
    let mut alter = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = alter
        .query(
            "SELECT id, operation FROM ir_access WHERE model_id=(SELECT id FROM ir_model WHERE model=$1) AND group_id IS NOT NULL",
            &[&"acceptance_notes.note"],
        )
        .await
        .unwrap();
    let permissions: Vec<(i64, String)> = rows.iter().map(|row| (row.get(0), row.get(1))).collect();
    assert!(!permissions.is_empty());
    alter
        .execute(
            "UPDATE ir_access SET operation='r' WHERE model_id=(SELECT id FROM ir_model WHERE model=$1) AND group_id IS NOT NULL",
            &[&"acceptance_notes.note"],
        )
        .await
        .unwrap();
    alter.commit().await.unwrap();
    drop(alter);
    let moved = database_generation(db, &policy).await;
    for (instance, before) in [&a, &b].into_iter().zip(applied_before) {
        assert!(moved > before, "the policy generation must have moved");
        assert_eq!(
            instance.signal().unwrap().applied().get(&policy),
            before,
            "no wake-up reached the instance"
        );
    }
    for service in [&a_service, &b_service] {
        let (_, readonly) = served_form_probe(service, &ordinary, form_view).await;
        assert_eq!(readonly, Some(true), "next request must see the new policy");
    }
    for (instance, before) in [&a, &b].into_iter().zip(applies_before) {
        assert_eq!(instance.signal().unwrap().applied().get(&policy), moved);
        assert!(instance.signal().unwrap().stats().applies > before);
    }
    let mut restore = orm.begin(SUPERUSER_ID).await.unwrap();
    for (id, operation) in permissions {
        restore
            .execute(
                "UPDATE ir_access SET operation=$1 WHERE id=$2",
                &[&operation, &id],
            )
            .await
            .unwrap();
    }
    restore.commit().await.unwrap();
    drop(restore);
    for service in [&a_service, &b_service] {
        let (_, readonly) = served_form_probe(service, &ordinary, form_view).await;
        assert_ne!(readonly, Some(true), "restored policy must be served");
    }

    // Menu change committed by instance A (named-cache invalidation); B has a warm cache.
    let before = served_menu_ids(&b_service, &ordinary).await;
    let mut tx = a.begin(SUPERUSER_ID).await.unwrap();
    let action = new_action(&mut tx, "Two-instance readable", "ir.config.setting").await;
    let menu = new_menu(
        &mut tx,
        "Two-instance menu",
        None,
        Some(format!("ir.actions.act_window,{action}")),
    )
    .await;
    tx.invalidate_ormcache(erp_addon_base::ir_ui_menu::MENUS_CACHE);
    tx.commit().await.unwrap();
    drop(tx);
    assert!(!before.contains(&menu));
    for service in [&a_service, &b_service] {
        assert!(served_menu_ids(service, &ordinary).await.contains(&menu));
    }
    let mut tx = a.begin(SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("ir.ui.menu")
        .unwrap()
        .browse([menu])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([action])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.invalidate_ormcache(erp_addon_base::ir_ui_menu::MENUS_CACHE);
    tx.commit().await.unwrap();
    drop(tx);
    for service in [&a_service, &b_service] {
        assert!(!served_menu_ids(service, &ordinary).await.contains(&menu));
    }
    write_test_arch(&a, form_view, &original).await;
}

async fn xmlid(tx: &mut erp_orm::Transaction, module: &str, name: &str) -> i64 {
    let rows = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module = $1 AND name = $2 AND model = $3",
            &[&module, &name, &"ir.ui.view"],
        )
        .await
        .unwrap();
    rows[0].get(0)
}

async fn relation_id(tx: &mut erp_orm::Transaction, name: &str, model: &str) -> i64 {
    tx.query(
        "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = $1 AND model = $2",
        &[&name, &model],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn model_xmlid(tx: &mut erp_orm::Transaction, module: &str, name: &str) -> i64 {
    tx.query(
        "SELECT res_id FROM ir_model_data WHERE module = $1 AND name = $2 AND model = 'ir.model'",
        &[&module, &name],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn create_user(
    tx: &mut erp_orm::Transaction,
    login: &str,
    password: &str,
    company: i64,
    groups: &[i64],
) {
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
        .await
        .unwrap();
    let company_ref = RecId::from_real(company).unwrap();
    let group_refs = groups
        .iter()
        .map(|id| RecId::from_real(*id).unwrap())
        .collect::<Vec<_>>();
    tx.env()
        .records("res.users")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(login.into()))
                    .set_cell("login", Cell::Str(login.into()))
                    .set_cell("password", Cell::Str(password.into()))
                    .set_cell("partner_id", Cell::Id(partner.ids()[0]))
                    .set_cell("company_id", Cell::Id(company))
                    .set_cell("lang", Cell::Str("en_US".into()))
                    .set_cell("tz", Cell::Str("UTC".into()))
                    .set_commands("company_ids", [Command::set([company_ref])])
                    .set_commands("group_ids", [Command::set(group_refs)]),
            ],
        )
        .await
        .unwrap();
}

async fn principal(auth: &Auth, login: &str, password: &str) -> erp_orm::Principal {
    let logged = auth
        .authenticate(LoginRequest::new(
            login,
            password,
            ClientAddr::InProcess("native-metadata-test"),
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
}

fn result(response: &erp_rpc::RpcResponse) -> serde_json::Value {
    serde_json::from_slice(response.body()).unwrap()
}

async fn stored_form(
    orm: &Orm,
    service: &RpcService,
    principal: erp_orm::Principal,
    view: i64,
    arch: &str,
) -> erp_rpc::RpcResponse {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE ir_ui_view SET arch_db = $1 WHERE id = $2",
        &[&arch, &view],
    )
    .await
    .unwrap();
    tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    tx.commit().await.unwrap();
    drop(tx);
    service.call(RpcCall::json2(
        "/json/2/acceptance_notes.note/get_views",
        serde_json::to_vec(&serde_json::json!({"views":[[view,"form"]],"wire_major":1,"options":{"mobile":false}})).unwrap(),
        Caller::Principal(principal),
    )).await
}

// Exercise static field occurrences through the installed RPC endpoint, rather than the
// XML parser alone. Model descriptors remain authoritative and caller access stays intact.
async fn assert_static_field_modifiers(
    orm: &Orm,
    service: &RpcService,
    ordinary: erp_orm::Principal,
    view: i64,
) {
    let arch = r#"<form><field name="name" readonly="1" required="0" force_save="True"/><field name="name" readonly="False" required="True" invisible="0"/><field name="body" invisible="1"/><field name="priority" required="1"/><field name="active"/><field name="id" readonly="0" force_save="1"/><field name="internal_memo" invisible="1" force_save="1"/><field name="body" groups="base.group_system" force_save="1"/></form>"#;
    let response = stored_form(orm, service, ordinary.clone(), view, arch).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body = result(&response);
    let nodes = body["views"][0]["nodes"].as_array().unwrap();
    let fields = &body["fields"]["acceptance_notes.note"];
    let literal = |value| serde_json::json!({"schema":1,"nodes":[{"kind":"literal","value":{"kind":"bool","value":value}}]});
    assert_eq!(
        nodes.len(),
        7,
        "only unauthorized field occurrences are pruned"
    );
    assert_eq!(
        nodes[1]["modifiers"],
        serde_json::json!({"readonly":literal(true),"required":literal(false),"invisible":null})
    );
    assert_eq!(nodes[1]["force_save"], true);
    assert_eq!(
        nodes[2]["modifiers"],
        serde_json::json!({"readonly":literal(false),"required":literal(true),"invisible":literal(false)})
    );
    assert_eq!(nodes[2]["force_save"], false);
    assert_eq!(nodes[3]["field"]["name"], "body");
    assert_eq!(nodes[3]["modifiers"]["invisible"], literal(true));
    assert_eq!(nodes[4]["modifiers"]["required"], literal(true));
    assert_eq!(
        nodes[5]["modifiers"],
        serde_json::json!({"readonly":null,"required":null,"invisible":null})
    );
    assert_eq!(nodes[5]["force_save"], false);
    assert_eq!(nodes[6]["field"]["name"], "id");
    // Explicit presentation overrides never rewrite the underlying model policy.
    assert_eq!(fields["name"]["required"], true);
    assert_eq!(fields["name"]["readonly"], false);
    assert_eq!(fields["priority"]["required"], false);
    assert_eq!(fields["id"]["readonly"], true);
    assert!(
        fields["body"].is_object(),
        "authorized invisible data stays available"
    );
    assert!(fields["internal_memo"].is_null());
    // extends the static contract with bounded record-snapshot expressions.
    for attribute in ["readonly", "required", "invisible"] {
        let arch =
            format!("<form><field name='name' {attribute}='active'/><field name='active'/></form>");
        let response = stored_form(orm, service, ordinary.clone(), view, &arch).await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        assert_eq!(
            result(&response)["views"][0]["nodes"][1]["modifiers"][attribute]["nodes"][0]["name"],
            "active"
        );
    }
    for (attribute, value) in [
        ("force_save", "active"),
        ("force_save", "2"),
        ("force_save", ""),
    ] {
        let arch = format!("<form><field name='name' {attribute}='{value}'/></form>");
        let response = stored_form(orm, service, ordinary.clone(), view, &arch).await;
        assert_eq!(response.status(), 422);
        let error = result(&response);
        assert_eq!(error["name"], "erp.exceptions.ValidationError");
        assert_eq!(
            error["message"],
            format!(
                "Dynamic '{attribute}' modifier is unsupported by the current human UI contract."
            )
        );
    }
}

async fn assert_field_widget_options(
    orm: &Orm,
    db: &TestDb,
    service: &RpcService,
    ordinary: erp_orm::Principal,
    admin: erp_orm::Principal,
    view: i64,
) {
    // Actual authorized numeric record persistence, separate from the browser fixture RPC.
    let spec = serde_json::json!({"rounding":{},"iso_numeric":{},"symbol":{},"position":{},"decimal_places":{}});
    let created = service.call(RpcCall::json2("/json/2/res.currency/web_save", serde_json::to_vec(&serde_json::json!({"ids":[],"vals":{"name":"XFW","symbol":"W","rounding":0.01,"iso_numeric":997,"position":"after"},"specification":spec})).unwrap(), Caller::Principal(admin.clone()))).await;
    assert_eq!(
        created.status(),
        200,
        "{}",
        String::from_utf8_lossy(created.body())
    );
    let created_body = result(&created);
    let numeric_id = created_body[0]["id"].as_i64().unwrap();
    assert_eq!(created_body[0]["rounding"], 0.01);
    assert_eq!(created_body[0]["iso_numeric"], 997);
    let saved = service.call(RpcCall::json2("/json/2/res.currency/web_save", serde_json::to_vec(&serde_json::json!({"ids":[numeric_id],"vals":{"rounding":0.05,"iso_numeric":998},"specification":spec})).unwrap(), Caller::Principal(admin))).await;
    assert_eq!(
        saved.status(),
        200,
        "{}",
        String::from_utf8_lossy(saved.body())
    );
    let read = service
        .call(RpcCall::json2(
            "/json/2/res.currency/web_read",
            serde_json::to_vec(&serde_json::json!({"ids":[numeric_id],"specification":spec}))
                .unwrap(),
            Caller::Principal(ordinary.clone()),
        ))
        .await;
    assert_eq!(
        read.status(),
        200,
        "{}",
        String::from_utf8_lossy(read.body())
    );
    assert_eq!(result(&read), result(&saved));
    assert_eq!(result(&read)[0]["rounding"], 0.05);
    assert_eq!(result(&read)[0]["iso_numeric"], 998);
    let mut numeric_tx = orm.begin(ordinary.subject()).await.unwrap();
    let numeric_error = numeric_tx
        .env()
        .records("res.currency")
        .unwrap()
        .browse([numeric_id])
        .unwrap()
        .check_access(&mut numeric_tx, erp_orm::Operation::Write)
        .await
        .unwrap_err();
    assert_eq!(numeric_error.kind(), erp_sdk::ErrorKind::Access);
    numeric_tx.rollback().await.unwrap();
    drop(numeric_tx);
    let denied_write = service.call(RpcCall::json2("/json/2/res.currency/web_save", serde_json::to_vec(&serde_json::json!({"ids":[numeric_id],"vals":{"rounding":0.1},"specification":spec})).unwrap(), Caller::Principal(ordinary.clone()))).await;
    assert_eq!(denied_write.status(), 403);
    assert_eq!(result(&denied_write)["name"], "erp.exceptions.AccessError");
    assert_eq!(result(&denied_write)["message"], numeric_error.message());
    let unchanged = service
        .call(RpcCall::json2(
            "/json/2/res.currency/web_read",
            serde_json::to_vec(&serde_json::json!({"ids":[numeric_id],"specification":spec}))
                .unwrap(),
            Caller::Principal(ordinary.clone()),
        ))
        .await;
    assert_eq!(unchanged.status(), 200);
    assert_eq!(result(&unchanged), result(&read));
    // Presentation uses this existing authorized RPC, never a privileged currency endpoint.
    let mut currency_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let company_id = ordinary.companies().current().unwrap().get();
    let currency_id: i64 = currency_tx
        .query(
            "SELECT currency_id FROM res_company WHERE id=$1",
            &[&company_id],
        )
        .await
        .unwrap()[0]
        .get(0);
    currency_tx.rollback().await.unwrap();
    drop(currency_tx);
    let currency_call = |principal| {
        RpcCall::json2("/json/2/res.currency/web_read", serde_json::to_vec(&serde_json::json!({"ids":[currency_id],"specification":{"symbol":{},"position":{},"decimal_places":{}}})).unwrap(), Caller::Principal(principal))
    };
    let currency = service.call(currency_call(ordinary.clone())).await;
    assert_eq!(
        currency.status(),
        200,
        "{}",
        String::from_utf8_lossy(currency.body())
    );
    let currency_rows = result(&currency);
    assert_eq!(currency_rows[0]["id"], currency_id);
    assert!(currency_rows[0]["symbol"].is_string());
    assert!(matches!(
        currency_rows[0]["position"].as_str(),
        Some("before" | "after")
    ));
    assert!(
        currency_rows[0]["decimal_places"]
            .as_u64()
            .is_some_and(|digits| digits <= 20)
    );
    let mut currency_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let currency_grants: Vec<i64> = currency_tx.query("SELECT id FROM ir_access WHERE active AND model_id=(SELECT id FROM ir_model WHERE model='res.currency') AND group_id IS NOT NULL", &[]).await.unwrap().iter().map(|row| row.get(0)).collect();
    assert!(!currency_grants.is_empty());
    currency_tx
        .execute(
            "UPDATE ir_access SET active=false WHERE id=ANY($1)",
            &[&currency_grants],
        )
        .await
        .unwrap();
    currency_tx.commit().await.unwrap();
    drop(currency_tx);
    let currency_denied_orm = fresh_metadata_policy_orm(orm, db).await;
    let mut currency_tx = currency_denied_orm.begin(ordinary.subject()).await.unwrap();
    let denied_error = currency_tx
        .env()
        .records("res.currency")
        .unwrap()
        .browse([currency_id])
        .unwrap()
        .check_access(&mut currency_tx, erp_orm::Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(denied_error.kind(), erp_sdk::ErrorKind::Access);
    currency_tx.rollback().await.unwrap();
    drop(currency_tx);
    let currency_denied_service = RpcService::new(currency_denied_orm, Host, RpcConfig::default());
    let currency_denied = currency_denied_service
        .call(currency_call(ordinary.clone()))
        .await;
    let mut currency_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    currency_tx
        .execute(
            "UPDATE ir_access SET active=true WHERE id=ANY($1)",
            &[&currency_grants],
        )
        .await
        .unwrap();
    currency_tx.commit().await.unwrap();
    drop(currency_tx);
    assert_eq!(currency_denied.status(), 403);
    assert_eq!(
        result(&currency_denied)["name"],
        "erp.exceptions.AccessError"
    );
    assert_eq!(result(&currency_denied)["message"], denied_error.message());
    let response = stored_form(orm, service, ordinary.clone(), view,
        r#"<form><field name="name" widget="password" placeholder="Secret" help="Private note" autocomplete="new-password"/></form>"#).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body = result(&response);
    let options = &body["views"][0]["nodes"][1]["options"];
    assert_eq!(options["placeholder"], "Secret");
    assert_eq!(options["help"], "Private note");
    assert_eq!(options["autocomplete"], "new-password");
    let scoped = stored_form(orm, service, ordinary.clone(), view,
        r#"<form><field name="company_id" domain="[('currency_id','!=',id)]" context="{'default_name':name}"/></form>"#).await;
    assert_eq!(
        scoped.status(),
        200,
        "{}",
        String::from_utf8_lossy(scoped.body())
    );
    let scoped_body = result(&scoped);
    assert!(scoped_body["views"][0]["nodes"][1]["options"]["domain"].is_object());
    assert!(scoped_body["fields"]["acceptance_notes.note"]["id"].is_object());
    assert!(scoped_body["fields"]["acceptance_notes.note"]["name"].is_object());
    assert!(scoped_body["fields"]["acceptance_notes.note"]["currency_id"].is_null());
    let private_rhs = stored_form(
        orm,
        service,
        ordinary.clone(),
        view,
        r#"<form><field name="company_id" domain="[('name','=',internal_memo)]"/></form>"#,
    )
    .await;
    assert_eq!(
        private_rhs.status(),
        403,
        "{}",
        String::from_utf8_lossy(private_rhs.body())
    );
    assert_eq!(result(&private_rhs)["name"], "erp.exceptions.AccessError");
    assert_eq!(
        result(&private_rhs)["message"],
        format!(
            "view {view}, line 1: the domain attribute reads the field 'internal_memo' of model 'acceptance_notes.note', which the user may not read"
        )
    );
    // Real compiled comodel Read denial must prune a relation-domain occurrence.
    let mut access = orm.begin(SUPERUSER_ID).await.unwrap();
    let grants: Vec<i64> = access.query("SELECT id FROM ir_access WHERE active AND model_id=(SELECT id FROM ir_model WHERE model='res.company') AND group_id IS NOT NULL", &[]).await.unwrap().iter().map(|row| row.get(0)).collect();
    assert!(!grants.is_empty());
    access
        .execute(
            "UPDATE ir_access SET active=false WHERE id=ANY($1)",
            &[&grants],
        )
        .await
        .unwrap();
    access.commit().await.unwrap();
    drop(access);
    let denied_orm = fresh_metadata_policy_orm(orm, db).await;
    let denied_service = RpcService::new(denied_orm.clone(), Host, RpcConfig::default());
    let denied_lhs = stored_form(
        &denied_orm,
        &denied_service,
        ordinary.clone(),
        view,
        r#"<form><field name="company_id" domain="[('currency_id','!=',False)]"/></form>"#,
    )
    .await;
    let mut access = orm.begin(SUPERUSER_ID).await.unwrap();
    access
        .execute(
            "UPDATE ir_access SET active=true WHERE id=ANY($1)",
            &[&grants],
        )
        .await
        .unwrap();
    access.commit().await.unwrap();
    drop(access);
    assert_eq!(
        denied_lhs.status(),
        200,
        "{}",
        String::from_utf8_lossy(denied_lhs.body())
    );
    assert_eq!(
        result(&denied_lhs)["views"][0]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "unreadable comodel criterion prunes its owning occurrence"
    );
    let denied = stored_form(
        orm,
        service,
        ordinary,
        view,
        r#"<form><field name="name" options="{'placeholder_field':'internal_memo'}"/></form>"#,
    )
    .await;
    assert_eq!(
        denied.status(),
        200,
        "{}",
        String::from_utf8_lossy(denied.body())
    );
    let body = result(&denied);
    assert!(body["fields"]["acceptance_notes.note"]["internal_memo"].is_null());
    assert_eq!(
        body["views"][0]["nodes"].as_array().unwrap().len(),
        1,
        "denied auxiliary dependency prunes its owning field occurrence"
    );
}

async fn assert_visible_menu_resolver_security(
    orm: &Orm,
    _service: &RpcService,
    admin: erp_orm::Principal,
    ordinary: erp_orm::Principal,
) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    // 1. Create a parent menu restricted to group_system, and a child menu with no groups
    let parent_menu = tx
        .env()
        .records("ir.ui.menu")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Admin Parent Menu".into()))
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    // Assign parent to base.group_system
    let sys_group = relation_id(&mut tx, "group_system", "res.groups").await;
    tx.execute(
        "INSERT INTO ir_ui_menu_group_rel (menu_id, gid) VALUES ($1, $2)",
        &[&parent_menu, &sys_group],
    )
    .await
    .unwrap();

    let child_menu = tx
        .env()
        .records("ir.ui.menu")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Ordinary Child Menu".into()))
                    .set_cell("parent_id", Cell::Id(parent_menu))
                    .set_cell("action", Cell::Str("ir.actions.act_window,1".into()))
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    // Create an inactive menu
    let inactive_menu = tx
        .env()
        .records("ir.ui.menu")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Inactive Menu".into()))
                    .set_cell("action", Cell::Str("ir.actions.act_window,1".into()))
                    .set_cell("active", Cell::Bool(false)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    tx.invalidate_ormcache(erp_addon_base::ir_ui_menu::MENUS_CACHE);
    tx.commit().await.unwrap();

    // Test with ordinary user
    let mut ord_tx = orm.begin(ordinary.subject()).await.unwrap();
    let ord_visible =
        erp_addon_base::ir_ui_menu::IrUiMenu::visible_menu_ids(&ord_tx.env(), &mut ord_tx, false)
            .await
            .unwrap();
    drop(ord_tx);

    assert!(
        !ord_visible.contains(&parent_menu),
        "parent restricted to admin must not be visible to ordinary user"
    );
    assert!(
        !ord_visible.contains(&child_menu),
        "child under denied parent must not be visible to ordinary user"
    );
    assert!(
        !ord_visible.contains(&inactive_menu),
        "inactive menu must not be visible"
    );

    // Test with admin user
    let mut adm_tx = orm.begin(admin.subject()).await.unwrap();
    let adm_visible =
        erp_addon_base::ir_ui_menu::IrUiMenu::visible_menu_ids(&adm_tx.env(), &mut adm_tx, false)
            .await
            .unwrap();
    drop(adm_tx);

    assert!(
        adm_visible.contains(&parent_menu),
        "admin must see parent menu"
    );
    assert!(
        adm_visible.contains(&child_menu),
        "admin must see child menu under parent"
    );
    assert!(
        !adm_visible.contains(&inactive_menu),
        "inactive menu must not be visible even to admin"
    );

    // Test denied-action parent closure: parent action points to admin-only model, child points to standard visible action.
    // Ensure child cannot resurrect the denied-action parent.
    let mut act_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    // Create an action with restricted groups (base.group_system)
    let admin_action = act_tx
        .env()
        .records("ir.actions.act_window")
        .unwrap()
        .create(
            &mut act_tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Admin Restricted Action".into()))
                    .set_cell("res_model", Cell::Str("res.users".into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    act_tx
        .execute(
            "INSERT INTO ir_act_window_group_rel (act_id, gid) VALUES ($1, $2)",
            &[&admin_action, &sys_group],
        )
        .await
        .unwrap();

    let denied_action_parent = act_tx
        .env()
        .records("ir.ui.menu")
        .unwrap()
        .create(
            &mut act_tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Denied Action Parent".into()))
                    .set_cell("sequence", Cell::Int(1000))
                    .set_cell(
                        "action",
                        Cell::Str(format!("ir.actions.act_window,{admin_action}").into()),
                    )
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    let child_under_denied_action = act_tx
        .env()
        .records("ir.ui.menu")
        .unwrap()
        .create(
            &mut act_tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Child Under Denied Action Parent".into()))
                    .set_cell("sequence", Cell::Int(0))
                    .set_cell("parent_id", Cell::Id(denied_action_parent))
                    .set_cell("action", Cell::Str("ir.actions.act_window,1".into()))
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];

    act_tx.invalidate_ormcache(erp_addon_base::ir_ui_menu::MENUS_CACHE);
    act_tx.commit().await.unwrap();

    let mut ord_tx2 = orm.begin(ordinary.subject()).await.unwrap();
    let ord_visible2 =
        erp_addon_base::ir_ui_menu::IrUiMenu::visible_menu_ids(&ord_tx2.env(), &mut ord_tx2, false)
            .await
            .unwrap();
    drop(ord_tx2);

    assert!(
        !ord_visible2.contains(&denied_action_parent),
        "parent with denied action must not be visible to ordinary user"
    );
    assert!(
        !ord_visible2.contains(&child_under_denied_action),
        "child under parent with denied action must not be visible to ordinary user"
    );

    // Cleanup
    let mut clean_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    clean_tx
        .env()
        .records("ir.ui.menu")
        .unwrap()
        .browse([
            child_menu,
            parent_menu,
            inactive_menu,
            denied_action_parent,
            child_under_denied_action,
        ])
        .unwrap()
        .unlink(&mut clean_tx)
        .await
        .unwrap();
    clean_tx
        .env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([admin_action])
        .unwrap()
        .unlink(&mut clean_tx)
        .await
        .unwrap();
    clean_tx.commit().await.unwrap();
}

async fn new_menu(
    tx: &mut erp_orm::Transaction,
    name: &str,
    parent: Option<i64>,
    action: Option<String>,
) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("active", Cell::Bool(true));
    if let Some(parent) = parent {
        vals = vals.set_cell("parent_id", Cell::Id(parent));
    }
    if let Some(action) = action {
        vals = vals.set_cell("action", Cell::Str(action.into()));
    }
    tx.env()
        .records("ir.ui.menu")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

async fn new_action(tx: &mut erp_orm::Transaction, name: &str, model: &str) -> i64 {
    tx.env()
        .records("ir.actions.act_window")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("res_model", Cell::Str(model.into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

async fn visible_menus(
    orm: &Orm,
    principal: &erp_orm::Principal,
    debug: bool,
) -> std::collections::BTreeSet<i64> {
    let mut tx = orm.begin(principal.subject()).await.unwrap();
    let ids = erp_addon_base::ir_ui_menu::IrUiMenu::visible_menu_ids(&tx.env(), &mut tx, debug)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    ids
}

/// installed fixture; and the debug rows are separate cases)
/// WM-04: every decision is made before any ancestor chain is walked.
async fn assert_menu_closure_cases(
    orm: &Orm,
    service: &RpcService,
    admin: erp_orm::Principal,
    ordinary: erp_orm::Principal,
) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let readable = new_action(&mut tx, "Closure readable", "ir.config.setting").await;
    // not `res.users`: an internal user reads their own row since My Preferences,
    // so the group model stands for "a model the caller cannot Read"
    let denied = new_action(&mut tx, "Closure denied model", "res.groups").await;
    let readable_ref = format!("ir.actions.act_window,{readable}");
    let ok_folder = new_menu(&mut tx, "Closure ok folder", None, None).await;
    let ok_child = new_menu(
        &mut tx,
        "Closure ok child",
        Some(ok_folder),
        Some(readable_ref),
    )
    .await;
    let missing_folder = new_menu(&mut tx, "Closure missing folder", None, None).await;
    // The ORM refuses dangling references on create, so the action is deleted afterwards and
    // the menu keeps the now-stale reference (a corrupt or half-removed record).
    let vanishing = new_action(&mut tx, "Closure vanishing", "ir.config.setting").await;
    let missing_child = new_menu(
        &mut tx,
        "Closure missing-action child",
        Some(missing_folder),
        Some(format!("ir.actions.act_window,{vanishing}")),
    )
    .await;
    for table in ["ir_act_window", "ir_actions"] {
        tx.execute(&format!("DELETE FROM {table} WHERE id = $1"), &[&vanishing])
            .await
            .unwrap();
    }
    let denied_folder = new_menu(&mut tx, "Closure denied folder", None, None).await;
    let denied_child = new_menu(
        &mut tx,
        "Closure denied-model child",
        Some(denied_folder),
        Some(format!("ir.actions.act_window,{denied}")),
    )
    .await;
    let empty_folder = new_menu(&mut tx, "Closure empty folder", None, None).await;
    tx.invalidate_ormcache(erp_addon_base::ir_ui_menu::MENUS_CACHE);
    tx.commit().await.unwrap();

    let ordinary_ids = visible_menus(orm, &ordinary, false).await;
    for id in [ok_folder, ok_child] {
        assert!(
            ordinary_ids.contains(&id),
            "readable chain must stay visible ({id})"
        );
    }
    for (id, why) in [
        (
            missing_folder,
            "folder whose only child has a missing action",
        ),
        (missing_child, "menu whose action record is missing"),
        (
            denied_folder,
            "folder whose only child targets a denied model",
        ),
        (
            denied_child,
            "menu whose action targets a model the caller cannot Read",
        ),
        (empty_folder, "empty folder"),
    ] {
        assert!(!ordinary_ids.contains(&id), "{why} must be pruned ({id})");
    }
    // The same closure over the RPC tree the web client receives.
    let tree = service
        .call(RpcCall::json2(
            "/json/2/ir.ui.menu/erp_load_web_menus",
            br#"{}"#.to_vec(),
            Caller::Principal(ordinary.clone()),
        ))
        .await;
    assert_eq!(
        tree.status(),
        200,
        "{}",
        String::from_utf8_lossy(tree.body())
    );
    let tree = result(&tree);
    let listed = |id: i64| {
        tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["id"] == id)
    };
    assert!(listed(ok_folder) && listed(ok_child), "{tree}");
    for id in [
        missing_folder,
        missing_child,
        denied_folder,
        denied_child,
        empty_folder,
    ] {
        assert!(!listed(id), "menu {id} must not be served; {tree}");
    }
    // The administrator can read the target model, so only the missing action and the empty
    // folder are pruned for them.
    let admin_ids = visible_menus(orm, &admin, false).await;
    assert!(admin_ids.contains(&denied_folder) && admin_ids.contains(&denied_child));
    for id in [missing_folder, missing_child, empty_folder] {
        assert!(
            !admin_ids.contains(&id),
            "pruned for the administrator too ({id})"
        );
    }

    // Debug on/off: the debug flag never grants model or action rights, so the closure is the
    // same either way. The technical-features row is NOT exercised here: the
    // fixture database does not seed `base.group_no_one`, and a membership change needs a
    // policy rebuild that this 4-connection test pool cannot afford (recorded in the receipts).
    for debug in [false, true] {
        let ids = visible_menus(orm, &ordinary, debug).await;
        assert!(
            ids.contains(&ok_folder) && ids.contains(&ok_child),
            "readable chain visible with debug={debug}"
        );
        for id in [
            denied_folder,
            denied_child,
            missing_folder,
            missing_child,
            empty_folder,
        ] {
            assert!(
                !ids.contains(&id),
                "debug={debug} must not grant access to menu {id}"
            );
        }
    }

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    // Cleanup: remove every fixture record.
    tx.env()
        .records("ir.ui.menu")
        .unwrap()
        .browse([
            ok_child,
            missing_child,
            denied_child,
            ok_folder,
            missing_folder,
            denied_folder,
            empty_folder,
        ])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([readable, denied])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.invalidate_ormcache(erp_addon_base::ir_ui_menu::MENUS_CACHE);
    tx.commit().await.unwrap();
}

async fn assert_addons_entry(
    orm: &Orm,
    service: &RpcService,
    admin: erp_orm::Principal,
    menu_id: i64,
    action_id: i64,
) {
    let menus = service
        .call(RpcCall::json2(
            "/json/2/ir.ui.menu/erp_load_web_menus",
            br#"{}"#.to_vec(),
            Caller::Principal(admin.clone()),
        ))
        .await;
    assert_eq!(
        menus.status(),
        200,
        "{}",
        String::from_utf8_lossy(menus.body())
    );
    let menu_tree = result(&menus);
    assert!(menu_tree["nodes"].as_array().unwrap().iter().any(|node| {
        node["id"] == menu_id && node["label"] == "Addons" && node["action_id"] == action_id
    }));

    // Second call returns cached menu tree
    let cached_menus = service
        .call(RpcCall::json2(
            "/json/2/ir.ui.menu/erp_load_web_menus",
            br#"{}"#.to_vec(),
            Caller::Principal(admin.clone()),
        ))
        .await;
    assert_eq!(cached_menus.status(), 200);
    assert_eq!(result(&cached_menus), menu_tree);

    // Writing a menu invalidates the cache
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("ir.ui.menu")
        .unwrap()
        .browse([menu_id])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("Addons Updated".into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let fresh_menus = service
        .call(RpcCall::json2(
            "/json/2/ir.ui.menu/erp_load_web_menus",
            br#"{}"#.to_vec(),
            Caller::Principal(admin.clone()),
        ))
        .await;
    assert_eq!(fresh_menus.status(), 200);
    let fresh_tree = result(&fresh_menus);
    assert!(
        fresh_tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| { node["id"] == menu_id && node["label"] == "Addons Updated" })
    );

    // Restore original menu name
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("ir.ui.menu")
        .unwrap()
        .browse([menu_id])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("Addons".into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let action = service
        .call(RpcCall::json2(
            "/json/2/ir.actions.act_window/erp_load_web_action",
            serde_json::to_vec(&serde_json::json!({"ids":[action_id]})).unwrap(),
            Caller::Principal(admin.clone()),
        ))
        .await;
    assert_eq!(action.status(), 200);
    assert_eq!(result(&action)["model"], "ir.module.module");

    let views = service
        .call(RpcCall::json2(
            "/json/2/ir.module.module/get_views",
            br#"{"views":[[null,"form"]],"wire_major":1,"options":{"mobile":false}}"#.to_vec(),
            Caller::Principal(admin.clone()),
        ))
        .await;
    assert_eq!(
        views.status(),
        200,
        "{}",
        String::from_utf8_lossy(views.body())
    );
    assert!(
        result(&views)["views"][0]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| { node["kind"] == "widget" && node["widget"] == "web.addons" })
    );

    // Second call to get_views returns identical cached result
    let cached_views = service
        .call(RpcCall::json2(
            "/json/2/ir.module.module/get_views",
            br#"{"views":[[null,"form"]],"wire_major":1,"options":{"mobile":false}}"#.to_vec(),
            Caller::Principal(admin.clone()),
        ))
        .await;
    assert_eq!(cached_views.status(), 200);
    assert_eq!(result(&cached_views), result(&views));
}

async fn assert_action_result_contract(
    orm: &Orm,
    service: &RpcService,
    admin: erp_orm::Principal,
    ordinary: erp_orm::Principal,
) {
    let window = serde_json::json!({
        "type": "ir.actions.act_window", "name": "Addon dialog",
        "res_model": "ir.module.module", "target": "new",
        "views": [[false, "form"]], "context": {"default_name": "draft-addon"}
    });
    let call = |principal, wire_major, value| {
        RpcCall::json2(
            // A readable source model must not grant read access to the result's target model.
            "/json/2/acceptance_notes.note/erp_normalize_action_result",
            serde_json::to_vec(&serde_json::json!({"wire_major": wire_major, "result": value}))
                .unwrap(),
            Caller::Principal(principal),
        )
    };
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let before: i64 = tx
        .query("SELECT count(*) FROM ir_module_module", &[])
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    drop(tx);
    let valid = service.call(call(admin.clone(), 1, window.clone())).await;
    assert_eq!(
        valid.status(),
        200,
        "{}",
        String::from_utf8_lossy(valid.body())
    );
    assert_eq!(
        result(&valid),
        serde_json::json!({
            "wire_major": 1,
            "result": {"kind": "window", "name": "Addon dialog", "model": "ir.module.module",
                "res_id": null, "views": [{"id": null, "type": "form"}],
                "context": {"default_name": "draft-addon"}}
        })
    );

    let current = serde_json::json!({"type":"ir.actions.act_window", "name":"Related modules",
        "res_model":"ir.module.module", "target":"current", "views":[[false,"list"],[false,"form"]],
        "domain":[["name","=","not-installed-test-module"]], "context":{"default_name":"draft-addon"}});
    let current_valid = service.call(call(admin.clone(), 1, current.clone())).await;
    assert_eq!(
        current_valid.status(),
        200,
        "{}",
        String::from_utf8_lossy(current_valid.body())
    );
    let normalized_current = result(&current_valid);
    assert_eq!(normalized_current["result"]["kind"], "current_window");
    assert_eq!(normalized_current["result"]["domain"], current["domain"]);
    // Empty result predicates do not bypass target-model Read admission.
    let current_denied = service.call(call(ordinary.clone(), 1, current)).await;
    assert_eq!(current_denied.status(), 403);

    let denied = service
        .call(call(ordinary.clone(), 1, window.clone()))
        .await;
    assert_eq!(
        denied.status(),
        403,
        "{}",
        String::from_utf8_lossy(denied.body())
    );
    let body = result(&denied);
    assert_eq!(body["name"], "erp.exceptions.AccessError");
    assert_eq!(
        body["message"],
        "Your access rights do not let you access 'Module' (ir.module.module) records.\n\n\
         Groups that may do this:\n\t- Settings Administration\n\n\
         Ask an administrator if you need this access."
    );
    assert_eq!(result(&current_denied)["name"], body["name"]);
    assert_eq!(result(&current_denied)["message"], body["message"]);

    assert_current_list_scope(orm, service, admin.clone(), ordinary.clone()).await;

    let mut unknown = window.clone();
    unknown["res_model"] = serde_json::json!("missing.wizard");
    let unknown = service.call(call(admin.clone(), 1, unknown)).await;
    assert_eq!(unknown.status(), 422);
    let body = result(&unknown);
    assert_eq!(body["name"], "erp.exceptions.ValidationError");
    assert_eq!(body["message"], "Unknown action model 'missing.wizard'.");

    let unsupported = service.call(call(admin, 2, window)).await;
    assert_eq!(unsupported.status(), 422);
    let body = result(&unsupported);
    assert_eq!(body["name"], "erp.exceptions.ValidationError");
    assert_eq!(body["message"], "Unsupported human UI wire major.");

    // A successful normalization does not create the proposed record or execute addon methods.
    // The endpoint is registered as read; JSON-2 therefore also enforces a read-only transaction.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let after: i64 = tx
        .query("SELECT count(*) FROM ir_module_module", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(after, before);
    tx.rollback().await.unwrap();
}

async fn assert_current_list_scope(
    orm: &Orm,
    service: &RpcService,
    admin: erp_orm::Principal,
    ordinary: erp_orm::Principal,
) {
    use serde_json::json;
    let company = ordinary.companies().current().unwrap().get();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let currency: i64 = tx
        .query(
            "SELECT currency_id FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()[0]
        .get(0);
    // Trusted fixture setup intentionally includes out-of-scope rows; all tested reads below
    // use authenticated non-System principals through JSON-2, never this setup environment.
    let other_company: i64 = tx
        .query(
            "INSERT INTO res_company(name,currency_id) VALUES($1,$2) RETURNING id",
            &[&"Action scope fixture company", &currency],
        )
        .await
        .unwrap()[0]
        .get(0);
    let mut ids = Vec::new();
    for (name, company, owner) in [
        ("Visible owned fixture", company, ordinary.subject().get()),
        ("Other owner fixture", company, admin.subject().get()),
        (
            "Other company fixture",
            other_company,
            ordinary.subject().get(),
        ),
    ] {
        ids.push(tx.query("INSERT INTO acceptance_notes_note(name,company_id,owner_id,active) VALUES($1,$2,$3,true) RETURNING id", &[&name,&company,&owner]).await.unwrap()[0].get::<_,i64>(0));
    }
    tx.commit().await.unwrap();
    drop(tx);
    let current = json!({"type":"ir.actions.act_window","name":"Scoped notes","res_model":"acceptance_notes.note","target":"current",
        "views":[[false,"list"],[false,"form"]],"domain":[["id","in",ids]],"context":{}});
    let normalize = |principal, value, companies: Vec<i64>| {
        RpcCall::json2(
            "/json/2/acceptance_notes.note/erp_normalize_action_result",
            serde_json::to_vec(
                &json!({"wire_major":1,"result":value,"context":{"allowed_company_ids":companies}}),
            )
            .unwrap(),
            Caller::Principal(principal),
        )
    };
    for (principal, expected) in [
        (ordinary.clone(), vec![ids[0]]),
        (admin.clone(), vec![ids[0], ids[1]]),
    ] {
        let accepted = service
            .call(normalize(principal.clone(), current.clone(), vec![company]))
            .await;
        assert_eq!(
            accepted.status(),
            200,
            "{}",
            String::from_utf8_lossy(accepted.body())
        );
        let action = result(&accepted);
        let rows = service.call(RpcCall::json2("/json/2/acceptance_notes.note/web_search_read",
            serde_json::to_vec(&json!({"domain":action["result"]["domain"],"specification":{"name":{}},"limit":40,"order":"id asc","context":{"allowed_company_ids":[company]}})).unwrap(),Caller::Principal(principal))).await;
        assert_eq!(
            rows.status(),
            200,
            "{}",
            String::from_utf8_lossy(rows.body())
        );
        let rows = result(&rows);
        let actual: Vec<i64> = rows["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(rows["length"].as_u64().unwrap(), expected.len() as u64);
    }
    let accepted_before_change = service
        .call(normalize(ordinary.clone(), current.clone(), vec![company]))
        .await;
    assert_eq!(accepted_before_change.status(), 200);
    let admitted_domain = result(&accepted_before_change)["result"]["domain"].clone();
    let mut change = orm.begin(SUPERUSER_ID).await.unwrap();
    change
        .execute(
            "UPDATE acceptance_notes_note SET owner_id=$1 WHERE id=$2",
            &[&admin.subject().get(), &ids[0]],
        )
        .await
        .unwrap();
    change.commit().await.unwrap();
    drop(change);
    let revoked = service.call(RpcCall::json2("/json/2/acceptance_notes.note/web_search_read",
        serde_json::to_vec(&json!({"domain":admitted_domain,"specification":{"name":{}},"context":{"allowed_company_ids":[company]}})).unwrap(),Caller::Principal(ordinary.clone()))).await;
    assert_eq!(revoked.status(), 200);
    assert_eq!(result(&revoked)["records"], json!([]));
    assert_eq!(result(&revoked)["length"], json!(0));
    let denied_company = service
        .call(normalize(
            ordinary.clone(),
            current.clone(),
            vec![other_company],
        ))
        .await;
    assert_eq!(denied_company.status(), 403);
    let refusal = result(&denied_company);
    assert_eq!(refusal["name"], "erp.exceptions.AccessError");
    assert_eq!(
        refusal["message"],
        erp_orm::PrincipalError::UnauthorizedCompanies.to_string()
    );
    let mut hidden = current;
    hidden["domain"] = json!([["internal_memo", "=", "secret"]]);
    let hidden_refusal = service
        .call(normalize(ordinary.clone(), hidden.clone(), vec![company]))
        .await;
    assert_eq!(hidden_refusal.status(), 403);
    let expected = service.call(RpcCall::json2("/json/2/acceptance_notes.note/web_search_read",serde_json::to_vec(&json!({"domain":hidden["domain"],"specification":{"name":{}},"context":{"allowed_company_ids":[company]}})).unwrap(),Caller::Principal(ordinary))).await;
    assert_eq!(expected.status(), 403);
    assert_eq!(result(&hidden_refusal)["name"], result(&expected)["name"]);
    assert_eq!(
        result(&hidden_refusal)["message"],
        result(&expected)["message"]
    );
}

#[test]
fn installed_metadata_rpc_uses_authenticated_acl_and_field_groups() {
    thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime
                .block_on(installed_metadata_rpc_uses_authenticated_acl_and_field_groups_async());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn assert_missing_search_view_is_loaded_by_upgrade(
    system: &ModuleSystem,
    orm: &Orm,
    db: &TestDb,
    search_view: i64,
) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("ir.ui.view")
        .unwrap()
        .browse([search_view])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let caller = system.system_env(erp_orm::Context::new());
    let names = [ModuleName::new("acceptance_notes").unwrap()];
    let upgraded = system
        .button_immediate_upgrade(&caller, &names)
        .await
        .unwrap();
    assert_eq!(upgraded.upgraded, names);
    assert!(
        upgraded.post_commit.is_clean(),
        "{:?}",
        upgraded.post_commit
    );
    // Module operations return a registry ORM; mount the same runtime dispatch/security as the server.
    let source = PolicySource::build(Arc::clone(upgraded.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let resolver = Resolver::for_registry(upgraded.orm.registry()).unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let upgraded_runtime = Orm::builder(Arc::clone(upgraded.orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(upgraded.orm.registry()),
            upgraded
                .orm
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
    let mut tx = upgraded_runtime.begin(SUPERUSER_ID).await.unwrap();
    let restored = xmlid(&mut tx, "acceptance_notes", "note_search").await;
    assert!(restored > 0);
    tx.rollback().await.unwrap();
    drop(tx);
    let upgraded_auth = Auth::new(
        upgraded_runtime.clone(),
        erp_security::auth::AuthConfig::DEFAULT,
        HashPool::global(),
    )
    .await
    .unwrap();
    let upgraded_principal =
        principal(&upgraded_auth, "metadata-admin", "metadata-admin-password").await;
    let upgraded_service = RpcService::new(upgraded_runtime, Host, RpcConfig::default());
    let response = upgraded_service.call(RpcCall::json2(
        "/json/2/acceptance_notes.note/get_views",
        serde_json::to_vec(&serde_json::json!({"views":[[null,"list"]],"wire_major":1,"options":{"mobile":false}})).unwrap(),
        Caller::Principal(upgraded_principal),
    )).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let description = result(&response);
    assert_eq!(
        description["views"][0]["nodes"][0]["search_panel"]["field"]["name"],
        "priority"
    );
    assert_eq!(
        description["views"][0]["nodes"][0]["search_panel"]["enable_counters"],
        true
    );
}

async fn assert_action_list_presentation(
    orm: &Orm,
    service: &RpcService,
    admin: erp_orm::Principal,
    ordinary: erp_orm::Principal,
    first_action: i64,
    list_view: i64,
) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let original: String = tx
        .query(
            "SELECT arch_db FROM ir_ui_view WHERE id = $1",
            &[&list_view],
        )
        .await
        .unwrap()[0]
        .get(0);
    let first_limit: i32 = tx
        .query(
            r#"SELECT "limit" FROM ir_act_window WHERE id = $1"#,
            &[&first_action],
        )
        .await
        .unwrap()[0]
        .get(0);
    let alternate_search = tx.env().records("ir.ui.view").unwrap().create(&mut tx, vec![RawVals::new()
        .set_cell("name", Cell::Str("Alternate list category".into()))
        .set_cell("model", Cell::Str("acceptance_notes.note".into()))
        .set_cell("priority", Cell::Int(99))
        .set_cell("arch_db", Cell::Str("<search><searchpanel><field name='priority' string='Second category' enable_counters='1' expand='1'/></searchpanel></search>".into()))]).await.unwrap();
    let second_action = tx
        .env()
        .records("ir.actions.act_window")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Second Notes action".into()))
                    .set_cell("res_model", Cell::Str("acceptance_notes.note".into()))
                    .set_cell("view_mode", Cell::Str("list,form".into()))
                    .set_cell("target", Cell::Str("current".into()))
                    .set_cell("search_view_id", Cell::Id(alternate_search.ids()[0]))
                    .set_cell("limit", Cell::Int(250)),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let call = |id| {
        RpcCall::json2(
            "/json/2/ir.actions.act_window/erp_load_web_action_views",
            serde_json::to_vec(
                &serde_json::json!({"ids":[id],"wire_major":1,"options":{"mobile":false}}),
            )
            .unwrap(),
            Caller::Principal(admin.clone()),
        )
    };
    let mut first_view_ids = None;
    for (id, expected_limit, label) in [
        (first_action, i64::from(first_limit), "Priority"),
        (second_action.ids()[0], 250, "Second category"),
    ] {
        let response = service.call(call(id)).await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let body = result(&response);
        if let Some(first) = &first_view_ids {
            assert_eq!(&body["action"]["views"], first);
        } else {
            first_view_ids = Some(body["action"]["views"].clone());
        }
        let view = body["views"]["views"]
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["type"] == "list")
            .unwrap();
        assert_eq!(view["nodes"][0]["page_limit"], expected_limit);
        assert_eq!(view["nodes"][0]["search_panel"]["label"], label);
    }
    write_test_arch(
        orm,
        list_view,
        "<list limit='50'><field name='name'/></list>",
    )
    .await;
    for id in [first_action, second_action.ids()[0]] {
        let response = service.call(call(id)).await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let body = result(&response);
        let view = body["views"]["views"]
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["type"] == "list")
            .unwrap();
        assert_eq!(view["nodes"][0]["page_limit"], 50);
    }
    write_test_arch(
        orm,
        list_view,
        "<list limit='0'><field name='name'/></list>",
    )
    .await;
    let response = service.call(call(second_action.ids()[0])).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body = result(&response);
    let view = body["views"]["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["type"] == "list")
        .unwrap();
    assert_eq!(view["nodes"][0]["page_limit"], 250);
    write_test_arch(orm, list_view, &original).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let system_group = relation_id(&mut tx, "group_system", "res.groups").await;
    alternate_search
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "group_ids",
                [Command::set([RecId::from_real(system_group).unwrap()])],
            ),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let denied = service.call(RpcCall::json2(
        "/json/2/ir.actions.act_window/erp_load_web_action_views",
        serde_json::to_vec(&serde_json::json!({"ids":[second_action.ids()[0]],"wire_major":1,"options":{"mobile":false}})).unwrap(),
        Caller::Principal(ordinary),
    )).await;
    assert_eq!(
        denied.status(),
        403,
        "{}",
        String::from_utf8_lossy(denied.body())
    );
    assert_eq!(result(&denied)["name"], "erp.exceptions.AccessError");
    assert_eq!(
        result(&denied)["message"],
        "You may not read the selected search view."
    );
    assert!(result(&denied).get("views").is_none());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    second_action
        .write(&mut tx, RawVals::new().set_cell("limit", Cell::Int(0)))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let invalid = service.call(call(second_action.ids()[0])).await;
    assert_eq!(invalid.status(), 422);
    assert_eq!(result(&invalid)["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        result(&invalid)["message"],
        "List page limit must be a positive browser-safe integer."
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    second_action.unlink(&mut tx).await.unwrap();
    alternate_search.unlink(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// Replace a stored search arch with raw SQL and invalidate the view template cache.
async fn write_search_arch(orm: &Orm, id: i64, arch: &str) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE ir_ui_view SET arch_db = $1 WHERE id = $2",
        &[&arch, &id],
    )
    .await
    .unwrap();
    tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    tx.commit().await.unwrap();
}

async fn write_test_arch(orm: &Orm, id: i64, arch: &str) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("ir.ui.view")
        .unwrap()
        .browse([id])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("arch_db", Cell::Str(arch.into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn assert_list_filter_metadata(
    orm: &Orm,
    service: &RpcService,
    ordinary: erp_orm::Principal,
    list_view: i64,
    search_view: i64,
) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let original_list: String = tx
        .query(
            "SELECT arch_db FROM ir_ui_view WHERE id = $1",
            &[&list_view],
        )
        .await
        .unwrap()[0]
        .get(0);
    let original_search: String = tx
        .query(
            "SELECT arch_db FROM ir_ui_view WHERE id = $1",
            &[&search_view],
        )
        .await
        .unwrap()[0]
        .get(0);
    let search_type = erp_addon_base::ir_ui_view::select::ViewType::new(&tx, "search").unwrap();
    assert_eq!(
        erp_addon_base::ir_ui_view::select::default_view(
            &mut tx,
            Some("acceptance_notes.note"),
            &search_type,
        )
        .await
        .unwrap()
        .map(|id| id.get()),
        Some(search_view),
        "installed XML search view is the actual default"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    write_test_arch(orm, list_view, "<list><field name='name'/></list>").await;
    let call = |context| {
        RpcCall::json2("/json/2/acceptance_notes.note/get_views", serde_json::to_vec(&serde_json::json!({"views":[[list_view,"list"]],"wire_major":1,"options":{"mobile":false},"context":context})).unwrap(), Caller::Principal(ordinary.clone()))
    };
    for (arch, kind, message) in [
        (
            "<search><searchpanel><field name='priority' enable_counters='1' expand='1'/></searchpanel></search>",
            Some("selection"),
            None,
        ),
        (
            r#"<search><field name='name'/><filter name='normal' string='Normal' domain="[('priority', '=', 'normal')]"/></search>"#,
            None,
            None,
        ),
        (
            "<search><searchpanel><field name='internal_memo'/></searchpanel></search>",
            None,
            None,
        ),
        (
            "<search><searchpanel><field name='priority' invisible='1'/></searchpanel></search>",
            None,
            None,
        ),
        (
            "<search><searchpanel view_types='kanban'><field name='priority'/></searchpanel></search>",
            None,
            None,
        ),
        (
            "<search><searchpanel><field name='priority' select='multi'/></searchpanel></search>",
            Some("sections"),
            None,
        ),
        (
            "<search><searchpanel><field name='name'/></searchpanel></search>",
            Some("unsupported"),
            Some(
                "Search panel selection category requires a static selection field. Supported search-panel extensions require a later release.",
            ),
        ),
        (
            "<search><searchpanel><field name='priority'/></searchpanel><searchpanel><field name='priority'/></searchpanel></search>",
            Some("validation"),
            Some("Search panel section 0 exceeds panel count limit."),
        ),
        (
            "<search><searchpanel bogus='1'><field name='priority'/></searchpanel></search>",
            Some("unsupported"),
            Some(
                "Search panel section 0 has unsupported panel syntax; additional syntax requires a later release.",
            ),
        ),
        (
            "<search><searchpanel><field name='priority' limit='1' enable_counters='1' expand='1'/></searchpanel></search>",
            Some("selection"),
            None,
        ),
    ] {
        write_test_arch(orm, search_view, arch).await;
        let response = service.call(call(serde_json::json!({}))).await;
        if kind == Some("validation") {
            assert_eq!(
                response.status(),
                422,
                "{}",
                String::from_utf8_lossy(response.body())
            );
            assert_eq!(result(&response)["name"], "erp.exceptions.ValidationError");
            assert_eq!(result(&response)["message"], message.unwrap());
            continue;
        }
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let body = result(&response);
        let panel = &body["views"][0]["nodes"][0]["search_panel"];
        if let Some(kind) = kind {
            assert_eq!(panel["kind"], kind, "search arch {arch}; response {body}");
            if kind == "sections" {
                assert_eq!(panel["sections"][0]["kind"], "filter");
                assert_eq!(panel["sections"][0]["id"], 1);
                assert_eq!(
                    panel["sections"][0]["field"],
                    serde_json::json!({"model":"acceptance_notes.note","name":"priority"})
                );
                assert!(body["fields"]["acceptance_notes.note"]["priority"].is_object());
            } else if let Some(message) = message {
                assert_eq!(panel["message"], message);
            } else {
                assert_eq!(
                    panel["field"],
                    serde_json::json!({"model":"acceptance_notes.note","name":"priority"})
                );
                assert_eq!(panel["enable_counters"], true);
                assert_eq!(panel["expand"], true);
                assert!(body["fields"]["acceptance_notes.note"]["priority"].is_object());
            }
        } else {
            assert!(body["views"][0]["nodes"][0].get("search_panel").is_none());
            assert!(
                body["fields"]["acceptance_notes.note"]
                    .get("internal_memo")
                    .is_none()
            );
        }
    }
    write_test_arch(
        orm,
        search_view,
        "<search><searchpanel><field name='priority' expand='1'/></searchpanel></search>",
    )
    .await;
    let no_counters = service.call(call(serde_json::json!({}))).await;
    assert_eq!(
        no_counters.status(),
        200,
        "{}",
        String::from_utf8_lossy(no_counters.body())
    );
    assert_eq!(
        result(&no_counters)["views"][0]["nodes"][0]["search_panel"]["enable_counters"],
        false
    );
    write_test_arch(orm, search_view, &original_search).await;
    let all_default = service
        .call(call(
            serde_json::json!({"searchpanel_default_priority":false}),
        ))
        .await;
    assert_eq!(all_default.status(), 200);
    assert!(
        result(&all_default)["views"][0]["nodes"][0]["search_panel"]["default_value"].is_null()
    );
    let default = service
        .call(call(
            serde_json::json!({"searchpanel_default_priority":"high"}),
        ))
        .await;
    assert_eq!(
        default.status(),
        200,
        "{}",
        String::from_utf8_lossy(default.body())
    );
    assert_eq!(
        result(&default)["views"][0]["nodes"][0]["search_panel"]["default_value"],
        "high"
    );
    let invalid = service
        .call(call(
            serde_json::json!({"searchpanel_default_priority":"unknown"}),
        ))
        .await;
    assert_eq!(invalid.status(), 422);
    assert_eq!(result(&invalid)["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        result(&invalid)["message"],
        "Search panel default is not a declared selection value."
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let extension = tx.env().records("ir.ui.view").unwrap().create(&mut tx, vec![RawVals::new()
        .set_cell("name", Cell::Str("Inherited priority label".into()))
        .set_cell("model", Cell::Str("acceptance_notes.note".into()))
        .set_cell("inherit_id", Cell::Id(search_view))
        .set_cell("arch_db", Cell::Str(r#"<xpath expr="//searchpanel/field[@name='priority']" position='attributes'><attribute name='string'>Priority states</attribute></xpath>"#.into()))]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let inherited = service.call(call(serde_json::json!({}))).await;
    assert_eq!(
        inherited.status(),
        200,
        "{}",
        String::from_utf8_lossy(inherited.body())
    );
    assert_eq!(
        result(&inherited)["views"][0]["nodes"][0]["search_panel"]["label"],
        "Priority states"
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    extension.unlink(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    write_test_arch(orm, search_view, &original_search).await;
    write_test_arch(orm, list_view, &original_list).await;
}

async fn assert_list_count_rpc_scope(
    orm: &Orm,
    service: &RpcService,
    ordinary: erp_orm::Principal,
    company: i64,
) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let owner: i64 = tx
        .query(
            "SELECT id FROM res_users WHERE login = 'metadata-user'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let other_owner: i64 = tx
        .query(
            "SELECT id FROM res_users WHERE login = 'metadata-admin'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let other_company: i64 = tx.query("INSERT INTO res_company(name, currency_id) SELECT 'Count other company', currency_id FROM res_company WHERE id = $1 RETURNING id", &[&company]).await.unwrap()[0].get(0);
    tx.execute("INSERT INTO acceptance_notes_note(name, priority, company_id, owner_id, active) VALUES ('count normal', 'normal', $1, $2, true), ('count unset', NULL, $1, $2, true), ('count hidden owner', 'high', $1, $3, true), ('count hidden company', 'high', $4, $2, true), ('count archived', 'high', $1, $2, false)", &[&company, &owner, &other_owner, &other_company]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let counts = service.call(RpcCall::json2(
        "/json/2/acceptance_notes.note/formatted_read_group",
        serde_json::to_vec(&serde_json::json!({"domain":[["name","ilike","count "]],"groupby":["priority"],"aggregates":["__count"],"context":{"allowed_company_ids":[company]}})).unwrap(),
        Caller::Principal(ordinary.clone()),
    )).await;
    assert_eq!(
        counts.status(),
        200,
        "{}",
        String::from_utf8_lossy(counts.body())
    );
    let mut actual = result(&counts)
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["priority"].clone(), row["__count"].clone()))
        .collect::<Vec<_>>();
    actual.sort_by_key(|(key, _)| key.to_string());
    let mut expected = vec![
        (serde_json::json!("normal"), serde_json::json!(1)),
        (serde_json::json!(false), serde_json::json!(1)),
    ];
    expected.sort_by_key(|(key, _)| key.to_string());
    assert_eq!(
        actual, expected,
        "owner and company rules exclude both hidden records; nullable state stays false"
    );
    let archived = service.call(RpcCall::json2(
        "/json/2/acceptance_notes.note/formatted_read_group",
        serde_json::to_vec(&serde_json::json!({"domain":[["active","=",false]],"groupby":["priority"],"aggregates":["__count"]})).unwrap(),
        Caller::Principal(ordinary.clone()),
    )).await;
    assert_eq!(
        archived.status(),
        200,
        "{}",
        String::from_utf8_lossy(archived.body())
    );
    let archived_groups = result(&archived);
    assert_eq!(archived_groups.as_array().unwrap().len(), 1);
    assert_eq!(archived_groups[0]["priority"], "high");
    assert_eq!(archived_groups[0]["__count"], 1);
    let rows = service.call(RpcCall::json2(
        "/json/2/acceptance_notes.note/web_search_read",
        serde_json::to_vec(&serde_json::json!({"domain":[["active","=",false]],"specification":{"name":{},"priority":{}},"offset":0,"limit":250})).unwrap(),
        Caller::Principal(ordinary.clone()),
    )).await;
    assert_eq!(
        rows.status(),
        200,
        "{}",
        String::from_utf8_lossy(rows.body())
    );
    assert_eq!(result(&rows)["length"], 1);
    assert_eq!(result(&rows)["records"][0]["name"], "count archived");
    let principal_for_comparison = ordinary.clone();
    let hidden_field = service.call(RpcCall::json2(
        "/json/2/acceptance_notes.note/formatted_read_group",
        serde_json::to_vec(&serde_json::json!({"domain":[],"groupby":["internal_memo"],"aggregates":["__count"]})).unwrap(),
        Caller::Principal(ordinary),
    )).await;
    assert_eq!(
        hidden_field.status(),
        403,
        "{}",
        String::from_utf8_lossy(hidden_field.body())
    );
    let direct_field = service.call(RpcCall::json2(
        "/json/2/acceptance_notes.note/web_search_read",
        serde_json::to_vec(&serde_json::json!({"domain":[],"specification":{"internal_memo":{}},"offset":0,"limit":40})).unwrap(),
        Caller::Principal(principal_for_comparison),
    )).await;
    assert_eq!(
        direct_field.status(),
        403,
        "{}",
        String::from_utf8_lossy(direct_field.body())
    );
    assert_eq!(result(&hidden_field)["name"], "erp.exceptions.AccessError");
    assert_eq!(
        result(&hidden_field)["message"],
        result(&direct_field)["message"]
    );
}

async fn installed_metadata_rpc_uses_authenticated_acl_and_field_groups_async() {
    // The independent H18 validator node has its own policy source; admit its connections explicitly.
    let db = TestDb::builder().pool_size(8).create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
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
            &erp_addon_uom::ADDON,
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_acceptance_notes::ADDON,
        ])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    // a database that serves audited calls carries the audit table; the
    // publication step installs it in a real deployment, this fixture does it directly.
    {
        let mut audit_tx = initialized
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
    let acceptance = ModuleName::new("acceptance_notes").unwrap();
    let install_env = system.system_env(erp_orm::Context::new());
    let installed = system
        .button_immediate_install(&install_env, &[acceptance])
        .await
        .unwrap();
    assert!(installed.post_commit.is_clean());

    // Build the runtime ORM from the installed module registry with the database's compiled
    // policy source, resolver and required authentication tables, as the native server does.
    let source = PolicySource::build(Arc::clone(installed.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let resolver = Resolver::for_registry(installed.orm.registry()).unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(installed.orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(installed.orm.registry()),
            installed
                .orm
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
    let company = relation_id(&mut tx, "main_company", "res.company").await;
    let system_group = relation_id(&mut tx, "group_system", "res.groups").await;
    let user_group = relation_id(&mut tx, "group_user", "res.groups").await;
    create_user(
        &mut tx,
        "metadata-admin",
        "metadata-admin-password",
        company,
        &[system_group],
    )
    .await;
    create_user(
        &mut tx,
        "metadata-user",
        "metadata-user-password",
        company,
        &[user_group],
    )
    .await;
    let list_view = xmlid(&mut tx, "acceptance_notes", "note_list").await;
    let form_view = xmlid(&mut tx, "acceptance_notes", "note_form").await;
    let search_view = xmlid(&mut tx, "acceptance_notes", "note_search").await;
    assert!(model_xmlid(&mut tx, "base", "model_ir_ui_menu").await > 0);
    assert!(model_xmlid(&mut tx, "base", "model_ir_actions_act_window").await > 0);
    assert!(model_xmlid(&mut tx, "acceptance_notes", "model_acceptance_notes_note").await > 0);
    let misplaced = tx
        .query(
            "SELECT count(*) FROM ir_model_data WHERE module = 'web' AND model = 'ir.model' \
             AND name IN ('model_ir_ui_menu', 'model_ir_actions_act_window')",
            &[],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    assert_eq!(
        misplaced, 0,
        "web extensions must not take base model XMLID ownership"
    );
    let action = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module = $1 AND name = $2 AND model = $3",
            &[
                &"acceptance_notes",
                &"note_action",
                &"ir.actions.act_window",
            ],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    let note_menu = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module = $1 AND name = $2 AND model = $3",
            &[&"acceptance_notes", &"notes_menu", &"ir.ui.menu"],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    let (addons_menu, addons_action) = tx
        .query(
            "SELECT menu.res_id, action.res_id FROM ir_model_data menu \
             JOIN ir_model_data action ON action.module = menu.module \
               AND action.name = 'addons_action' AND action.model = 'ir.actions.act_window' \
             WHERE menu.module = 'web' AND menu.name = 'addons_menu' AND menu.model = 'ir.ui.menu'",
            &[],
        )
        .await
        .unwrap()
        .first()
        .map(|row| (row.get::<_, i64>(0), row.get::<_, i64>(1)))
        .unwrap();
    tx.env()
        .records("ir.actions.act_window")
        .unwrap()
        .browse([action])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "group_ids",
                [Command::set([RecId::from_real(system_group).unwrap()])],
            ),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let admin = principal(&auth, "metadata-admin", "metadata-admin-password").await;
    let ordinary = principal(&auth, "metadata-user", "metadata-user-password").await;
    assert_h18_generation_validator_is_used(&orm, &db, admin.clone(), list_view).await;
    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
    assert_readonly_access_floors(&orm, &db, &service, ordinary.clone(), form_view).await;
    assert_search_field_operator_contract(&orm, &db, &service, ordinary.clone(), search_view).await;
    assert_two_serving_instances_share_database(&orm, &db, ordinary.clone(), form_view).await;
    assert_builtin_handles_name_direct_root_fields(
        &orm,
        &db,
        ordinary.clone(),
        list_view,
        search_view,
    )
    .await;
    assert_list_count_rpc_scope(&orm, &service, ordinary.clone(), company).await;
    assert_list_filter_metadata(&orm, &service, ordinary.clone(), list_view, search_view).await;
    panel_contract_custom::run(&orm, &db, ordinary.clone(), admin.clone(), search_view).await;
    assert_action_list_presentation(
        &orm,
        &service,
        admin.clone(),
        ordinary.clone(),
        action,
        list_view,
    )
    .await;

    assert_action_result_contract(&orm, &service, admin.clone(), ordinary.clone()).await;
    assert_action_bundle(
        &orm,
        &service,
        admin.clone(),
        ordinary.clone(),
        action,
        form_view,
    )
    .await;

    let menu_call = |principal| {
        RpcCall::json2(
            "/json/2/ir.ui.menu/erp_load_web_menus",
            br#"{}"#.to_vec(),
            Caller::Principal(principal),
        )
    };
    let menus = service.call(menu_call(admin.clone())).await;
    assert_eq!(
        menus.status(),
        200,
        "{}",
        String::from_utf8_lossy(menus.body())
    );
    assert_addons_entry(&orm, &service, admin.clone(), addons_menu, addons_action).await;
    assert_visible_menu_resolver_security(&orm, &service, admin.clone(), ordinary.clone()).await;
    assert_menu_closure_cases(&orm, &service, admin.clone(), ordinary.clone()).await;

    let action_call = |principal| {
        RpcCall::json2(
            "/json/2/ir.actions.act_window/erp_load_web_action",
            serde_json::to_vec(&serde_json::json!({"ids":[action]})).unwrap(),
            Caller::Principal(principal),
        )
    };
    let action_response = service.call(action_call(admin.clone())).await;
    assert_eq!(
        action_response.status(),
        200,
        "{}",
        String::from_utf8_lossy(action_response.body())
    );
    assert_eq!(result(&action_response)["model"], "acceptance_notes.note");

    let view_body =
        br#"{"views":[[null,"list"],[null,"form"]],"wire_major":1,"options":{"mobile":false}}"#
            .to_vec();
    let view_call = |principal| {
        RpcCall::json2(
            "/json/2/acceptance_notes.note/get_views",
            view_body.clone(),
            Caller::Principal(principal),
        )
    };
    let admin_views = service.call(view_call(admin.clone())).await;
    assert_eq!(
        admin_views.status(),
        200,
        "{}",
        String::from_utf8_lossy(admin_views.body())
    );
    assert!(result(&admin_views)["fields"]["acceptance_notes.note"]["internal_memo"].is_object());
    let user_views = service.call(view_call(ordinary.clone())).await;
    assert_eq!(
        user_views.status(),
        200,
        "{}",
        String::from_utf8_lossy(user_views.body())
    );
    if let Some(dir) = std::env::var_os("ERP_STATUSBAR_FIXTURE_DIR") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("acceptance-notes-note.json"),
            serde_json::to_vec_pretty(&result(&user_views)).unwrap(),
        )
        .unwrap();
    }
    assert!(result(&user_views)["fields"]["acceptance_notes.note"]["internal_memo"].is_null());
    assert!(result(&user_views)["fields"]["acceptance_notes.note"]["name"].is_object());

    assert_button_context_caller_admission(&orm, &db, admin.clone(), ordinary.clone(), form_view)
        .await;
    assert_served_wire_attributes(&orm, &service, ordinary.clone(), form_view, action).await;

    let mut layout_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    layout_tx.execute(
        "UPDATE ir_ui_view SET arch_db = $1 WHERE id = $2",
        &[&r#"<form><sheet><group string="Details"><field name="name"/></group></sheet></form>"#, &form_view],
    ).await.unwrap();
    layout_tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    layout_tx.commit().await.unwrap();
    drop(layout_tx);
    let layout = service.call(view_call(ordinary.clone())).await;
    assert_eq!(
        layout.status(),
        200,
        "{}",
        String::from_utf8_lossy(layout.body())
    );
    let layout_body = result(&layout);
    let form = layout_body["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["nodes"][0]["kind"] == "form")
        .unwrap();
    assert_eq!(form["nodes"][0]["children"], serde_json::json!([1]));
    assert_eq!(
        form["nodes"][1],
        serde_json::json!({"kind":"sheet","children":[2]})
    );
    assert_eq!(
        form["nodes"][2],
        serde_json::json!({"kind":"group","label":"Details","columns":2,"colspan":2,"children":[3]})
    );
    assert_eq!(form["nodes"][3]["field"]["name"], "name");

    assert_static_field_modifiers(&orm, &service, ordinary.clone(), form_view).await;
    assert_field_widget_options(
        &orm,
        &db,
        &service,
        ordinary.clone(),
        admin.clone(),
        form_view,
    )
    .await;

    // Static presentation hiding cannot make an unknown model field valid.
    let unknown_hidden = stored_form(&orm, &service, ordinary.clone(), form_view,
        r#"<form><notebook><page name="hidden" invisible="True"><field name="unknown_hidden_field"/></page></notebook></form>"#).await;
    assert_eq!(unknown_hidden.status(), 422);
    assert_eq!(
        result(&unknown_hidden)["name"],
        "erp.exceptions.ValidationError"
    );
    assert_eq!(
        result(&unknown_hidden)["message"],
        format!(
            "Field \"unknown_hidden_field\" does not exist in model \"acceptance_notes.note\" (view {form_view}, line 1)"
        )
    );

    let protected_arch = r#"<form><sheet><group><field name="name"/></group><group groups="base.group_system" string="Admin"><field name="body"/></group><notebook><page name="general"><field name="priority"/></page><page name="secret" groups="base.group_system"><field name="internal_memo"/></page><page name="hidden" invisible="True"><field name="body"/></page></notebook><separator string="Fin"/></sheet><aside groups="base.group_system"><group string="Record information"><field name="create_date"/><field name="internal_memo"/></group></aside></form>"#;
    let protected = stored_form(&orm, &service, ordinary.clone(), form_view, protected_arch).await;
    assert_eq!(
        protected.status(),
        200,
        "{}",
        String::from_utf8_lossy(protected.body())
    );
    let protected = result(&protected);
    let fields = protected["fields"]["acceptance_notes.note"]
        .as_object()
        .unwrap();
    assert_eq!(
        fields.keys().map(String::as_str).collect::<Vec<_>>(),
        ["name", "priority"]
    );
    let nodes = protected["views"][0]["nodes"].as_array().unwrap();
    assert!(!nodes.iter().any(|node| node["label"] == "Admin"
        || node["name"] == "secret"
        || node["name"] == "hidden"
        || node["kind"] == "aside"));
    let admin = principal(&auth, "metadata-admin", "metadata-admin-password").await;
    let allowed = stored_form(&orm, &service, admin.clone(), form_view, protected_arch).await;
    assert_eq!(
        allowed.status(),
        200,
        "{}",
        String::from_utf8_lossy(allowed.body())
    );
    let allowed = result(&allowed);
    assert!(allowed["fields"]["acceptance_notes.note"]["body"].is_object());
    assert!(allowed["fields"]["acceptance_notes.note"]["internal_memo"].is_object());
    assert!(allowed["fields"]["acceptance_notes.note"]["unknown_hidden_field"].is_null());
    assert!(allowed["fields"]["acceptance_notes.note"]["create_date"].is_object());
    assert!(
        allowed["views"][0]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "aside")
    );

    // A visible rail still cannot disclose a field restricted at model-field level.
    let rail = stored_form(&orm, &service, ordinary.clone(), form_view,
        r#"<form><sheet><field name="name"/></sheet><aside><group string="Record information"><field name="create_date"/><field name="write_date"/><field name="internal_memo"/></group></aside></form>"#).await;
    assert_eq!(
        rail.status(),
        200,
        "{}",
        String::from_utf8_lossy(rail.body())
    );
    let rail = result(&rail);
    assert!(rail["fields"]["acceptance_notes.note"]["create_date"].is_object());
    assert!(rail["fields"]["acceptance_notes.note"]["write_date"].is_object());
    assert!(rail["fields"]["acceptance_notes.note"]["internal_memo"].is_null());
    assert!(
        !rail["views"][0]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["field"]["name"] == "internal_memo")
    );

    // Inherited XML can add a real-data rail; get_views combines and authorizes it.
    let base_response = stored_form(
        &orm,
        &service,
        ordinary.clone(),
        form_view,
        "<form><sheet><field name='name'/></sheet></form>",
    )
    .await;
    assert_eq!(
        base_response.status(),
        200,
        "{}",
        String::from_utf8_lossy(base_response.body())
    );

    // Verify search view support in get_views and field inclusion
    let search_call = |principal| {
        RpcCall::json2(
            "/json/2/acceptance_notes.note/get_views",
            serde_json::to_vec(&serde_json::json!({
                "views": [[null, "search"]],
                "wire_major": 1,
                "options": {"mobile": false}
            }))
            .unwrap(),
            Caller::Principal(principal),
        )
    };
    let search_response = service.call(search_call(ordinary.clone())).await;
    assert_eq!(
        search_response.status(),
        200,
        "{}",
        String::from_utf8_lossy(search_response.body())
    );
    let search_result = result(&search_response);
    let search_view_node = search_result["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["type"] == "search")
        .expect("search view returned");
    assert_eq!(search_view_node["nodes"][0]["kind"], "search");
    // When search is requested, all readable fields of the model are described
    assert!(search_result["fields"]["acceptance_notes.note"]["name"].is_object());
    assert!(search_result["fields"]["acceptance_notes.note"]["priority"].is_object());
    assert!(search_result["fields"]["acceptance_notes.note"]["internal_memo"].is_null());

    // Search view custom arch verification: group filtering, searchpanel refusal, leaf children
    let search_view_id = xmlid(
        &mut orm.begin(SUPERUSER_ID).await.unwrap(),
        "acceptance_notes",
        "note_search",
    )
    .await;
    let arch: &str = r#"<search string="Search Notes">
            <field name="name" filter_domain="['|', ('name', 'ilike', self), ('body', 'ilike', self)]"/>
            <filter name="in_filter" string="Priority In" domain="[('priority', 'in', ['low', 'high'])]"/>
            <searchpanel>
                <field name="priority" select="one"/>
            </searchpanel>
            <field name="internal_memo" groups="base.group_system"/>
            <filter name="my_notes" string="My Notes" domain="[('name','=','test')]"/>
            <filter name="admin_notes" string="Admin Notes" domain="[('name','=','admin')]" groups="base.group_system"/>
            <separator/>
            <separator groups="base.group_system"/>
            <field name="priority" invisible="1"/>
            <filter name="invisible_filter" string="Invisible" domain="[('name','=','inv')]" invisible="True"/>
            <filter name="unreadable_domain_filter" string="Unreadable Domain" domain="[('internal_memo','=','secret')]"/>
            <group string="Group By">
                <filter name="group_priority" string="Priority" context="{'group_by':'priority'}"/>
                <filter name="group_secret" string="Secret" context="{'group_by':'internal_memo'}" groups="base.group_system"/>
                <filter name="ungrouped_secret" string="Ungrouped Secret" context="{'group_by':'internal_memo'}"/>
            </group>
        </search>"#;
    write_search_arch(&orm, search_view_id, arch).await;
    // An unguarded filter whose domain reads a field the caller may not read survives pruning, so
    // the whole view is refused with a located error (plan Host step: surviving prohibited
    // references refuse). The earlier fixture expected the filter to be pruned.
    let refused = service.call(search_call(ordinary.clone())).await;
    assert_eq!(
        refused.status(),
        403,
        "{}",
        String::from_utf8_lossy(refused.body())
    );
    assert_eq!(result(&refused)["name"], "erp.exceptions.AccessError");
    assert_eq!(
        result(&refused)["message"],
        format!(
            "view {search_view_id}, line 14: the domain attribute reads the field 'internal_memo' of model 'acceptance_notes.note', which the user may not read"
        )
    );
    // Guarding the same filters (domain and context) by group prunes them for the ordinary caller and keeps it for admin.
    let guarded = arch.replace(
        "domain=\"[('internal_memo','=','secret')]\"",
        "domain=\"[('internal_memo','=','secret')]\" groups=\"base.group_system\"",
    );
    let guarded = guarded.replace(
        "string=\"Ungrouped Secret\" context=\"{'group_by':'internal_memo'}\"",
        "string=\"Ungrouped Secret\" context=\"{'group_by':'internal_memo'}\" groups=\"base.group_system\"",
    );
    assert_eq!(
        guarded.matches("groups=\"base.group_system\"").count(),
        arch.matches("groups=\"base.group_system\"").count() + 2
    );
    write_search_arch(&orm, search_view_id, &guarded).await;

    let ordinary_search = service.call(search_call(ordinary.clone())).await;
    assert_eq!(
        ordinary_search.status(),
        200,
        "{}",
        String::from_utf8_lossy(ordinary_search.body())
    );
    let ord_search_res = result(&ordinary_search);
    let ord_view = ord_search_res["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["type"] == "search")
        .unwrap();
    let ord_nodes = ord_view["nodes"].as_array().unwrap();
    assert!(
        ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_field" && n["field"]["name"] == "name")
    );
    assert!(
        !ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_field" && n["field"]["name"] == "internal_memo")
    );
    assert!(
        ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "in_filter")
    );
    assert!(
        ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "my_notes")
    );
    assert!(
        !ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "admin_notes")
    );
    assert!(
        !ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "group_secret")
    );
    assert!(
        !ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_field" && n["field"]["name"] == "priority")
    );
    assert!(
        !ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "invisible_filter")
    );
    assert!(
        !ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "unreadable_domain_filter")
    );
    assert!(
        !ord_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "ungrouped_secret")
    );

    let admin_search = service.call(search_call(admin.clone())).await;
    assert_eq!(admin_search.status(), 200);
    let adm_search_res = result(&admin_search);
    let adm_view = adm_search_res["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["type"] == "search")
        .unwrap();
    let adm_nodes = adm_view["nodes"].as_array().unwrap();
    assert!(
        adm_nodes
            .iter()
            .any(|n| n["kind"] == "search_field" && n["field"]["name"] == "internal_memo")
    );
    assert!(
        adm_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "admin_notes")
    );
    assert!(
        adm_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "group_secret")
    );
    assert!(
        adm_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "unreadable_domain_filter")
    );
    assert!(
        adm_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "ungrouped_secret")
    );

    // Refusal of toolbar option naming
    let toolbar_call = RpcCall::json2(
        "/json/2/acceptance_notes.note/get_views",
        serde_json::to_vec(&serde_json::json!({
            "views": [[null, "search"]],
            "wire_major": 1,
            "options": {"mobile": false, "toolbar": true}
        }))
        .unwrap(),
        Caller::Principal(ordinary.clone()),
    );
    let toolbar_res = service.call(toolbar_call).await;
    assert_eq!(toolbar_res.status(), 422);
    let toolbar_err = result(&toolbar_res);
    assert_eq!(toolbar_err["name"], "erp.exceptions.ValidationError");
    assert!(toolbar_err["message"].as_str().unwrap().contains(
        "Option 'toolbar' is unsupported by a later release; deferred to a later release."
    ));

    // Refusal of children under leaf search nodes (<field>)
    let mut leaf_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    leaf_tx
        .execute(
            "UPDATE ir_ui_view SET arch_db = $1 WHERE id = $2",
            &[
                &r#"<search><field name="name"><filter name="bad"/></field></search>"#,
                &search_view_id,
            ],
        )
        .await
        .unwrap();
    leaf_tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    leaf_tx.commit().await.unwrap();
    drop(leaf_tx);
    let leaf_res = service.call(search_call(ordinary.clone())).await;
    assert_eq!(leaf_res.status(), 422);
    let leaf_err = result(&leaf_res);
    assert_eq!(leaf_err["name"], "erp.exceptions.ValidationError");
    assert!(
        leaf_err["message"]
            .as_str()
            .unwrap()
            .contains("Field nodes cannot contain children.")
    );

    // Refusal of unknown field reference in filter domain
    let mut unknown_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    unknown_tx
        .execute(
            "UPDATE ir_ui_view SET arch_db = $1 WHERE id = $2",
            &[
                &r#"<search><filter name="bad_field" string="Bad" domain="[('nonexistent_field','=',1)]"/></search>"#,
                &search_view_id,
            ],
        )
        .await
        .unwrap();
    unknown_tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    unknown_tx.commit().await.unwrap();
    drop(unknown_tx);
    let unknown_res = service.call(search_call(ordinary.clone())).await;
    assert_eq!(unknown_res.status(), 422);
    let unknown_err = result(&unknown_res);
    assert_eq!(unknown_err["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        unknown_err["message"],
        format!(
            "Field \"nonexistent_field\" used in the domain attribute does not exist in model \"acceptance_notes.note\" (view {search_view_id}, line 1)"
        )
    );

    // Support for list-form criteria, dotted relation traversal, group_by list, and context/self
    let mut ast_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    ast_tx
        .execute(
            "UPDATE ir_ui_view SET arch_db = $1 WHERE id = $2",
            &[
                &r#"<search>
                    <filter name="list_form" string="List Form" domain="[['name', '=', 'test']]"/>
                    <filter name="dotted_domain" string="Company Name" domain="[('company_id.name', '=', 'My Company')]"/>
                    <filter name="self_context" string="Context Filter" domain="[('name', '=', context.get('active_name'))]"/>
                    <searchpanel><field name="priority" expand="1"/></searchpanel>
                    <group string="Expanded Group" expand="1">
                        <filter name="gb_list" string="Group List" context="{'group_by': ['priority', 'create_date:month']}"/>
                    </group>
                </search>"#,
                &search_view_id,
            ],
        )
        .await
        .unwrap();
    ast_tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    ast_tx.commit().await.unwrap();
    drop(ast_tx);
    let ast_res = service.call(search_call(ordinary.clone())).await;
    assert_eq!(
        ast_res.status(),
        200,
        "{}",
        String::from_utf8_lossy(ast_res.body())
    );
    let ast_body = result(&ast_res);
    let ast_view = ast_body["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["type"] == "search")
        .unwrap();
    let ast_nodes = ast_view["nodes"].as_array().unwrap();
    assert!(
        ast_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "list_form")
    );
    assert!(
        ast_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "dotted_domain")
    );
    assert!(
        ast_nodes
            .iter()
            .any(|n| n["kind"] == "search_filter" && n["name"] == "self_context")
    );
    let exp_group = ast_nodes
        .iter()
        .find(|n| n["kind"] == "search_group" && n["label"] == "Expanded Group")
        .expect("group found");
    assert_eq!(exp_group["expand"], true);
    assert_eq!(
        ast_nodes[0]["search_panel"]["kind"], "selection",
        "standalone Search must preserve its admitted panel"
    );
    assert_eq!(ast_nodes[0]["search_panel"]["expand"], true);

    let mut invalid_op_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    invalid_op_tx.execute("UPDATE ir_ui_view SET arch_db=$1 WHERE id=$2", &[&r#"<search><filter name="invalid_op" domain="[('name', 'unknown_operator', 1)]"/></search>"#, &search_view_id]).await.unwrap();
    invalid_op_tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    invalid_op_tx.commit().await.unwrap();
    drop(invalid_op_tx);
    let invalid_op = service.call(search_call(ordinary.clone())).await;
    assert_eq!(
        invalid_op.status(),
        422,
        "{}",
        String::from_utf8_lossy(invalid_op.body())
    );
    let invalid_op_body = result(&invalid_op);
    assert_eq!(invalid_op_body["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        invalid_op_body["message"],
        "Unsupported search domain operator 'unknown_operator'. Additional operators require a later release."
    );

    // Refusal of unknown dotted field reference
    let mut bad_dot_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    bad_dot_tx
        .execute(
            "UPDATE ir_ui_view SET arch_db = $1 WHERE id = $2",
            &[
                &r#"<search><filter name="bad_dot" string="Bad Dot" domain="[('company_id.nonexistent_subfield', '=', 'test')]"/></search>"#,
                &search_view_id,
            ],
        )
        .await
        .unwrap();
    bad_dot_tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    bad_dot_tx.commit().await.unwrap();
    drop(bad_dot_tx);
    let bad_dot_res = service.call(search_call(ordinary.clone())).await;
    assert_eq!(bad_dot_res.status(), 422);
    let bad_dot_err = result(&bad_dot_res);
    assert_eq!(bad_dot_err["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        bad_dot_err["message"],
        format!(
            "Field \"nonexistent_subfield\" used in the domain attribute does not exist in model \"res.company\" (view {search_view_id}, line 1)"
        )
    );

    // Restore standard search view arch
    let mut restore_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    restore_tx.execute(
        "UPDATE ir_ui_view SET arch_db = $1 WHERE id = $2",
        &[&r#"<search string="Search Notes"><field name="name"/><field name="priority"/></search>"#, &search_view_id],
    ).await.unwrap();
    restore_tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    restore_tx.commit().await.unwrap();
    drop(restore_tx);
    let mut inherited_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let extension = inherited_tx.env().records("ir.ui.view").unwrap().create(
        &mut inherited_tx,
        vec![RawVals::new()
            .set_cell("name", Cell::Str("note.record.information.extension".into()))
            .set_cell("model", Cell::Str("acceptance_notes.note".into()))
            .set_cell("inherit_id", Cell::Id(form_view))
            .set_cell("arch", Cell::Str(r#"<xpath expr="//sheet" position="after"><aside><group string="Inherited record information"><field name="create_date"/><field name="internal_memo"/></group></aside></xpath>"#.into()))]
    ).await.unwrap().ids()[0];
    inherited_tx.commit().await.unwrap();
    // Committed fixture transactions retain their pool lease until dropped.
    drop(inherited_tx);
    let inherited = service.call(view_call(ordinary.clone())).await;
    assert_eq!(
        inherited.status(),
        200,
        "{}",
        String::from_utf8_lossy(inherited.body())
    );
    let inherited = result(&inherited);
    // view_call requests list first, form second; select the form by its actual type.
    let inherited_form = inherited["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["type"] == "form")
        .expect("get_views returned the requested form");
    assert!(
        inherited_form["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["label"] == "Inherited record information"),
        "fresh inherited form: {inherited}"
    );
    assert!(inherited["fields"]["acceptance_notes.note"]["create_date"].is_object());
    assert!(inherited["fields"]["acceptance_notes.note"]["internal_memo"].is_null());
    let mut cleanup_tx = orm.begin(SUPERUSER_ID).await.unwrap();
    cleanup_tx
        .env()
        .records("ir.ui.view")
        .unwrap()
        .browse([extension])
        .unwrap()
        .unlink(&mut cleanup_tx)
        .await
        .unwrap();
    cleanup_tx.commit().await.unwrap();
    drop(cleanup_tx);

    for (arch, expected) in [
        (
            "<form><notebook><page name='same'/><page name='same'/></notebook></form>".to_owned(),
            "Duplicate notebook page name 'same'. Unsupported form layout requires a later release.",
        ),
        (
            "<form><group col='3'/></form>".to_owned(),
            "<group> col must be an even integer from 2 through 12. Unsupported form layout requires a later release.",
        ),
        (
            format!(
                "<form>{}{}</form>",
                "<group>".repeat(32),
                "</group>".repeat(32)
            ),
            "View exceeds the 32-level depth limit.",
        ),
        (
            format!("<form>{}</form>", "<separator/>".repeat(4096)),
            "Requested views exceed the 4096-node aggregate limit.",
        ),
    ] {
        let response = stored_form(&orm, &service, ordinary.clone(), form_view, &arch).await;
        assert_eq!(
            response.status(),
            422,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let error = result(&response);
        assert_eq!(error["name"], "erp.exceptions.ValidationError");
        assert_eq!(error["message"], expected);
    }
    let title = stored_form(&orm, &service, ordinary.clone(), form_view,
        r#"<form><sheet><div class="oe_title"><h1><field name="name"/></h1><h2><field name="internal_memo"/></h2></div><group><field name="body"/></group></sheet></form>"#).await;
    assert_eq!(
        title.status(),
        200,
        "{}",
        String::from_utf8_lossy(title.body())
    );
    let title = result(&title);
    assert!(title["fields"]["acceptance_notes.note"]["name"].is_object());
    assert!(title["fields"]["acceptance_notes.note"]["internal_memo"].is_null());
    assert!(
        title["views"][0]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["kind"] == "title")
    );
    assert!(
        !title["views"][0]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["field"]["name"] == "internal_memo")
    );

    let statusbar = stored_form(&orm, &service, ordinary.clone(), form_view,
        "<form><header><field name='priority' widget='statusbar' statusbar_visible='low, high' readonly='1'/></header><sheet><field name='name'/></sheet></form>").await;
    assert_eq!(
        statusbar.status(),
        200,
        "{}",
        String::from_utf8_lossy(statusbar.body())
    );
    let statusbar = result(&statusbar);
    let node = &statusbar["views"][0]["nodes"][2];
    assert_eq!(node["field"]["name"], "priority");
    assert_eq!(node["widget"], "statusbar");
    assert_eq!(
        node["options"],
        serde_json::json!({"visible":["low","high"]})
    );
    assert_eq!(
        statusbar["fields"]["acceptance_notes.note"]["priority"]["selection"],
        serde_json::json!([["low", "Low"], ["normal", "Normal"], ["high", "High"]])
    );
    for (arch, expected) in [
        (
            "<form><header><field name='priority' widget='statusbar' statusbar_visible='low,low'/></header></form>",
            "Duplicate statusbar_visible selection key 'low'.",
        ),
        (
            "<form><header><field name='priority' widget='statusbar' statusbar_visible='missing'/></header></form>",
            "Unknown statusbar_visible selection key 'missing'.",
        ),
        (
            "<form><header><field name='priority' widget='statusbar' statusbar_visible=''/></header></form>",
            "statusbar_visible must contain nonempty selection keys.",
        ),
        (
            "<form><header><field name='name' widget='statusbar'/></header></form>",
            "Field widget 'statusbar' does not support type 'char'.",
        ),
        (
            "<form><field name='priority' widget='statusbar'/></form>",
            "The statusbar widget requires a record form header.",
        ),
        (
            "<form><field name='priority' statusbar_visible='low'/></form>",
            "statusbar_visible requires widget='statusbar'.",
        ),
    ] {
        let response = stored_form(&orm, &service, ordinary.clone(), form_view, arch).await;
        assert_eq!(
            response.status(),
            422,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let error = result(&response);
        assert_eq!(error["name"], "erp.exceptions.ValidationError");
        assert_eq!(error["message"], expected);
    }

    // Header object buttons: the method must be RPC-exposed on the model when the view is
    // described (partial). `priority` is a field of the note.
    let header = |button: &str| {
        format!(
            "<form><header>{button}</header><sheet><field name='name'/><field name='priority'/></sheet></form>"
        )
    };
    let rendered = stored_form(
        &orm,
        &service,
        ordinary.clone(),
        form_view,
        &header(
            "<button name='action_archive' type='object' string='Archive' confirm='Archive it?' invisible=\"priority == 'high'\"/>",
        ),
    )
    .await;
    assert_eq!(
        rendered.status(),
        200,
        "{}",
        String::from_utf8_lossy(rendered.body())
    );
    let nodes = result(&rendered)["views"][0]["nodes"].clone();
    let kinds: Vec<_> = nodes
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        kinds,
        ["form", "header", "button", "sheet", "field", "field"]
    );
    assert_eq!(nodes[2]["name"], "action_archive");
    assert_eq!(nodes[2]["confirm"], "Archive it?");
    assert!(nodes[2]["modifiers"]["invisible"].is_object());
    for (button, expected) in [
        (
            "<button name='no_such_method' type='object' string='X'/>",
            "Button 'no_such_method' names a method that 'acceptance_notes.note' does not expose over RPC. Unsupported form layout requires a later release.".to_owned(),
        ),
        (
            "<button name='sudo' type='object' string='X'/>",
            "Button 'sudo' names a method that 'acceptance_notes.note' does not expose over RPC. Unsupported form layout requires a later release.".to_owned(),
        ),
        (
            "<button name='action_archive' type='object' string='X' invisible=\"missing == 1\"/>",
            format!(
                "Field \"missing\" used in the invisible attribute does not exist in model \"acceptance_notes.note\" (view {form_view}, line 1)"
            ),
        ),
    ] {
        let response =
            stored_form(&orm, &service, ordinary.clone(), form_view, &header(button)).await;
        assert_eq!(
            response.status(),
            422,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        assert_eq!(result(&response)["message"], expected);
    }
    // Actual numeric record value, not an invented related-record count. The object method
    // is the same exposed archive action used by ordinary header buttons above.
    let smart = |extra: &str| {
        format!(
            "<form><sheet><div name='button_box' class='oe_button_box'><button name='action_archive' type='object' class='oe_stat_button' {extra}><field name='id' widget='statinfo' string='Record number'/></button></div><field name='name'/></sheet></form>"
        )
    };
    let response = stored_form(&orm, &service, ordinary.clone(), form_view, &smart("")).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let nodes = result(&response)["views"][0]["nodes"].clone();
    assert_eq!(nodes[2]["kind"], "button_box");
    assert_eq!(nodes[3]["kind"], "stat_button");
    assert_eq!(nodes[3]["children"], serde_json::json!([4]));
    assert_eq!(nodes[4]["field"]["name"], "id");
    assert_eq!(nodes[4]["widget"], "statinfo");
    assert!(nodes[4]["modifiers"]["readonly"].is_object());
    for extra in ["groups='base.group_system'", "invisible='1'"] {
        let response =
            stored_form(&orm, &service, ordinary.clone(), form_view, &smart(extra)).await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        assert!(
            !result(&response)["views"][0]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["kind"] == "stat_button")
        );
    }
    // A real stored extension inserts its action into the same named box through get_views.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let extension = tx.env().records("ir.ui.view").unwrap().create(&mut tx, vec![RawVals::new()
        .set_cell("name", Cell::Str("metadata.smart.extension".into()))
        .set_cell("model", Cell::Str("acceptance_notes.note".into()))
        .set_cell("inherit_id", Cell::Id(form_view))
        .set_cell("arch_db", Cell::Str(r#"<xpath expr="//div[@name='button_box']" position="inside"><button name="action_unarchive" type="object" class="oe_stat_button" string="Restore"><field name="id" widget="statinfo" string="Record number"/></button></xpath>"#.into()))
    ]).await.unwrap().ids()[0];
    tx.commit().await.unwrap();
    drop(tx);
    let response = stored_form(&orm, &service, ordinary.clone(), form_view, &smart("")).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let nodes = result(&response)["views"][0]["nodes"].clone();
    let actions: Vec<_> = nodes
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["kind"] == "stat_button")
        .map(|node| node["name"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["action_archive", "action_unarchive"]);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("DELETE FROM ir_ui_view WHERE id = $1", &[&extension])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // Each requested view fits by itself; the response's aggregate includes both roots.
    let boundary = format!("<form>{}</form>", "<separator/>".repeat(4095));
    let single = stored_form(&orm, &service, ordinary.clone(), form_view, &boundary).await;
    assert_eq!(
        single.status(),
        200,
        "{}",
        String::from_utf8_lossy(single.body())
    );
    assert_eq!(
        result(&single)["views"][0]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        4096
    );
    let aggregate = service.call(view_call(ordinary.clone())).await;
    assert_eq!(aggregate.status(), 422);
    assert_eq!(result(&aggregate)["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        result(&aggregate)["message"],
        "Requested views exceed the 4096-node aggregate limit."
    );

    let bundle_admin = principal(&auth, "metadata-admin", "metadata-admin-password").await;
    let bundle_error = service
        .call(RpcCall::json2(
            "/json/2/ir.actions.act_window/erp_load_web_action_views",
            serde_json::to_vec(
                &serde_json::json!({"ids":[action],"wire_major":1,"options":{"mobile":false}}),
            )
            .unwrap(),
            Caller::Principal(bundle_admin),
        ))
        .await;
    assert_eq!(bundle_error.status(), 422);
    assert_eq!(
        result(&bundle_error)["name"],
        "erp.exceptions.ValidationError"
    );
    assert_eq!(
        result(&bundle_error)["message"],
        result(&aggregate)["message"]
    );
    assert!(result(&bundle_error).get("action").is_none());

    // An oversized stored architecture is refused by the database preflight before the ORM
    // fetches its payload. This simulates corrupt/legacy data that bypassed the normal write cap.
    let oversized_tx = &mut orm.begin(SUPERUSER_ID).await.unwrap();
    let max_catalogue_bytes = ViewLimits::default().get(LimitKind::CatalogueBytes);
    let oversized_arch_bytes = i32::try_from(max_catalogue_bytes + 1).unwrap();
    oversized_tx
        .execute(
            "UPDATE ir_ui_view SET arch_db = repeat('x', $1) WHERE id = $2",
            &[&oversized_arch_bytes, &form_view],
        )
        .await
        .unwrap();
    let actual_catalogue_bytes:i64=oversized_tx.query("SELECT octet_length(arch_db)::bigint + 2 * octet_length(name)::bigint + 2 * octet_length(model)::bigint + octet_length(type)::bigint + octet_length(mode)::bigint FROM ir_ui_view WHERE id=$1",&[&form_view]).await.unwrap()[0].get(0);
    assert!(usize::try_from(actual_catalogue_bytes).unwrap() > max_catalogue_bytes);
    oversized_tx.invalidate_ormcache(erp_addon_base::ir_ui_view::TEMPLATES);
    oversized_tx.commit().await.unwrap();
    let admin = principal(&auth, "metadata-admin", "metadata-admin-password").await;
    let oversized = service.call(view_call(admin)).await;
    assert_eq!(
        oversized.status(),
        422,
        "{}",
        String::from_utf8_lossy(oversized.body())
    );
    let oversized_body = result(&oversized);
    assert_eq!(oversized_body["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        oversized_body["message"],
        format!(
            "the text of the inheritance trees this check reads (the trees of all its written views together) is {actual_catalogue_bytes} bytes or more, more than the {max_catalogue_bytes} allowed"
        )
    );

    let bundle_admin = principal(&auth, "metadata-admin", "metadata-admin-password").await;
    let bundle_error = service
        .call(RpcCall::json2(
            "/json/2/ir.actions.act_window/erp_load_web_action_views",
            serde_json::to_vec(
                &serde_json::json!({"ids":[action],"wire_major":1,"options":{"mobile":false}}),
            )
            .unwrap(),
            Caller::Principal(bundle_admin),
        ))
        .await;
    assert_eq!(
        bundle_error.status(),
        422,
        "oversized bundled-view RPC: status={} wire_error_kind={:?} body={}",
        bundle_error.status(),
        serde_json::from_slice::<serde_json::Value>(bundle_error.body())
            .map(|body| body.get("name").cloned()),
        String::from_utf8_lossy(bundle_error.body())
    );
    assert_eq!(
        result(&bundle_error)["name"],
        "erp.exceptions.ValidationError"
    );
    assert_eq!(
        result(&bundle_error)["message"],
        result(&oversized)["message"]
    );
    assert!(result(&bundle_error).get("action").is_none());

    let oversized_text_tx = &mut orm.begin(SUPERUSER_ID).await.unwrap();
    oversized_text_tx
        .execute(
            "UPDATE ir_act_window SET context = repeat('x', $1) WHERE id = $2",
            &[&4097_i32, &action],
        )
        .await
        .unwrap();
    oversized_text_tx
        .execute(
            "UPDATE ir_ui_menu SET name = repeat('x', $1) WHERE id = $2",
            &[&4097_i32, &note_menu],
        )
        .await
        .unwrap();
    oversized_text_tx.commit().await.unwrap();
    let admin = principal(&auth, "metadata-admin", "metadata-admin-password").await;
    let oversized_action = service.call(action_call(admin.clone())).await;
    assert_eq!(oversized_action.status(), 422);
    let oversized_action_body = result(&oversized_action);
    assert_eq!(
        oversized_action_body["name"],
        "erp.exceptions.ValidationError"
    );
    assert_eq!(
        oversized_action_body["message"],
        "Window action context exceeds the 4096-byte limit."
    );
    let bundle_admin = principal(&auth, "metadata-admin", "metadata-admin-password").await;
    let bundle_error = service
        .call(RpcCall::json2(
            "/json/2/ir.actions.act_window/erp_load_web_action_views",
            serde_json::to_vec(
                &serde_json::json!({"ids":[action],"wire_major":1,"options":{"mobile":false}}),
            )
            .unwrap(),
            Caller::Principal(bundle_admin),
        ))
        .await;
    assert_eq!(bundle_error.status(), 422);
    assert_eq!(
        result(&bundle_error)["name"],
        "erp.exceptions.ValidationError"
    );
    assert_eq!(
        result(&bundle_error)["message"],
        result(&oversized_action)["message"]
    );
    assert!(result(&bundle_error).get("action").is_none());

    let oversized_menu = service.call(menu_call(admin.clone())).await;
    assert_eq!(oversized_menu.status(), 422);
    let oversized_menu_body = result(&oversized_menu);
    assert_eq!(
        oversized_menu_body["name"],
        "erp.exceptions.ValidationError"
    );
    assert_eq!(
        oversized_menu_body["message"],
        "Menu label exceeds the 4096-byte limit."
    );

    let denied = service.call(action_call(ordinary)).await;
    assert_eq!(
        denied.status(),
        403,
        "{}",
        String::from_utf8_lossy(denied.body())
    );
    assert_missing_search_view_is_loaded_by_upgrade(&system, &orm, &db, search_view).await;
    drop(service);
    drop(auth);
    drop(orm);
    drop(installed);
    drop(initialized);
    drop(system);
    db.cleanup().await.unwrap();
}

// UI.ACTION-BUNDLE: same caller-bound metadata as the existing separate calls.
async fn assert_action_bundle(
    orm: &Orm,
    service: &RpcService,
    admin: erp_orm::Principal,
    ordinary: erp_orm::Principal,
    action_id: i64,
    form_view: i64,
) {
    let call = |method: &str, body: serde_json::Value, principal| {
        RpcCall::json2(
            &format!("/json/2/ir.actions.act_window/{method}"),
            serde_json::to_vec(&body).unwrap(),
            Caller::Principal(principal),
        )
    };
    let args = serde_json::json!({"ids":[action_id],"wire_major":1,"options":{"mobile":false}});
    for stored in [false, true] {
        if stored {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            tx.execute(
                "UPDATE ir_act_window SET view_id = $1 WHERE id = $2",
                &[&form_view, &action_id],
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        let bundle = service
            .call(call(
                "erp_load_web_action_views",
                args.clone(),
                admin.clone(),
            ))
            .await;
        assert_eq!(
            bundle.status(),
            200,
            "{}",
            String::from_utf8_lossy(bundle.body())
        );
        let action = service
            .call(call(
                "erp_load_web_action",
                serde_json::json!({"ids":[action_id]}),
                admin.clone(),
            ))
            .await;
        assert_eq!(action.status(), 200);
        let action = result(&action);
        let requests: Vec<_> = action["views"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| serde_json::json!([v["id"], v["type"]]))
            .collect();
        let views = service.call(RpcCall::json2(&format!("/json/2/{}/get_views",action["model"].as_str().unwrap()),
        serde_json::to_vec(&serde_json::json!({"views":requests,"wire_major":1,"options":{"mobile":false}})).unwrap(), Caller::Principal(admin.clone()))).await;
        assert_eq!(views.status(), 200);
        let mut expected_views = result(&views);
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let declared_limit: i32 = tx
            .query(
                r#"SELECT "limit" FROM ir_act_window WHERE id = $1"#,
                &[&action_id],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.rollback().await.unwrap();
        drop(tx);
        let bundle = result(&bundle);
        let mut action_owned_limit = false;
        for view in expected_views["views"].as_array_mut().unwrap() {
            if view["type"] == "list" && view["nodes"][0].get("page_limit").is_none() {
                view["nodes"][0]["page_limit"] = serde_json::json!(declared_limit);
                action_owned_limit = true;
            }
        }
        // Action-owned presentation changes the metadata identity, while form-only bundles retain it.
        assert_eq!(
            bundle["views"]["description_version"] == expected_views["description_version"],
            !action_owned_limit
        );
        expected_views["description_version"] = bundle["views"]["description_version"].clone();
        assert_eq!(
            bundle,
            serde_json::json!({"action":action,"views":expected_views})
        );
    }
    let denied = service
        .call(call(
            "erp_load_web_action_views",
            args.clone(),
            ordinary.clone(),
        ))
        .await;
    assert_eq!(denied.status(), 403);
    assert_eq!(result(&denied)["name"], "erp.exceptions.AccessError");
    assert_eq!(
        result(&denied)["message"],
        "You may not read this window action."
    );
    assert!(result(&denied).get("action").is_none());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let denied_action = tx
        .env()
        .records("ir.actions.act_window")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Restricted target".into()))
                    .set_cell("res_model", Cell::Str("ir.module.module".into()))
                    .set_cell("view_mode", Cell::Str("form".into()))
                    .set_cell("target", Cell::Str("current".into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    let target_denial = service
        .call(call(
            "erp_load_web_action",
            serde_json::json!({"ids":[denied_action]}),
            ordinary.clone(),
        ))
        .await;
    let denied = service
        .call(call(
            "erp_load_web_action_views",
            serde_json::json!({"ids":[denied_action],"wire_major":1,"options":{"mobile":false}}),
            ordinary,
        ))
        .await;
    assert_eq!(target_denial.status(), 403);
    assert_eq!(denied.status(), 403);
    assert_eq!(result(&denied)["name"], "erp.exceptions.AccessError");
    assert_eq!(
        result(&denied)["message"],
        result(&target_denial)["message"]
    );
    assert!(
        result(&denied)["message"]
            .as_str()
            .unwrap()
            .contains("(ir.module.module)")
    );
    assert!(result(&denied).get("action").is_none());
    for body in [
        serde_json::json!({"ids":[action_id],"wire_major":1,"options":{}}),
        serde_json::json!({"ids":[action_id],"wire_major":1}),
    ] {
        let response = service
            .call(call("erp_load_web_action_views", body, admin.clone()))
            .await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let envelope = result(&response);
        assert_eq!(envelope["action"]["id"], action_id);
        assert_eq!(envelope["views"]["wire_major"], 1);
    }
    for (body, message) in [
        (
            serde_json::json!({"ids":[],"wire_major":1,"options":{"mobile":false}}),
            "erp_load_web_action requires exactly one window action.",
        ),
        (
            serde_json::json!({"ids":[action_id],"wire_major":2,"options":{"mobile":false}}),
            "Unsupported human UI wire major.",
        ),
        (
            serde_json::json!({"ids":[action_id],"wire_major":1,"options":{"mobile":"false"}}),
            "get_views option 'mobile' must be boolean.",
        ),
        (
            serde_json::json!({"ids":[action_id],"wire_major":1,"options":{"mobile":false,"extra":true}}),
            "Unknown get_views option 'extra'.",
        ),
    ] {
        let response = service
            .call(call("erp_load_web_action_views", body, admin.clone()))
            .await;
        assert_eq!(
            response.status(),
            422,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        assert_eq!(result(&response)["name"], "erp.exceptions.ValidationError");
        assert_eq!(result(&response)["message"], message);
        assert!(result(&response).get("action").is_none());
    }
}
