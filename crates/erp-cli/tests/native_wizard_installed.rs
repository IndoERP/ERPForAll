//! Native installed wizard acceptance: same SDK declarations as the component fixture.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/login_rollout.rs"]
mod login_rollout;

use std::{path::Path, sync::Arc, thread};

use erp_fields::{Cell, RecId};
use erp_modules::{
    AddonIndex, ModuleName,
    system::{InitOptions, ModuleSystem},
};
use erp_orm::{Command, RawVals, SUPERUSER_ID};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

// `#[method(wire, rpc)]` on the component fixture declares package exposure only;
// native exposure requires `#[rpc]` (erp-macros/src/methods.rs). Supply the six
// object methods referenced by the shipped book, loan and wizard forms, without
// changing the addon. Both runtimes below use the same explicit table.
static NATIVE_WIZARD_RPC: [erp_orm::rpc::RpcImpl; 6] = [
    erp_orm::rpc::RpcImpl {
        addon: "sample_library_wasm",
        model: "sample_library.book",
        method: "open_loans",
        decl: erp_orm::rpc::RpcDecl::Expose {
            effect: erp_orm::rpc::Effect::Read,
            receiver: erp_orm::rpc::Receiver::Records,
            params: &[],
            bind: |tx, records, _args| native_wizard_method(tx, records, "open_loans"),
        },
        file: file!(),
        line: line!(),
    },
    erp_orm::rpc::RpcImpl {
        addon: "sample_library_wasm",
        model: "sample_library.book",
        method: "lend",
        decl: erp_orm::rpc::RpcDecl::Expose {
            effect: erp_orm::rpc::Effect::Write,
            receiver: erp_orm::rpc::Receiver::Records,
            params: &[],
            bind: |tx, records, _args| native_wizard_method(tx, records, "lend"),
        },
        file: file!(),
        line: line!(),
    },
    erp_orm::rpc::RpcImpl {
        addon: "sample_library_wasm",
        model: "sample_library.book",
        method: "give_back",
        decl: erp_orm::rpc::RpcDecl::Expose {
            effect: erp_orm::rpc::Effect::Write,
            receiver: erp_orm::rpc::Receiver::Records,
            params: &[],
            bind: |tx, records, _args| native_return_method(tx, records),
        },
        file: file!(),
        line: line!(),
    },
    erp_orm::rpc::RpcImpl {
        addon: "sample_library_wasm",
        model: "sample_library.loan",
        method: "give_back",
        decl: erp_orm::rpc::RpcDecl::Expose {
            effect: erp_orm::rpc::Effect::Write,
            receiver: erp_orm::rpc::Receiver::Records,
            params: &[],
            bind: |tx, records, _args| native_return_method(tx, records),
        },
        file: file!(),
        line: line!(),
    },
    erp_orm::rpc::RpcImpl {
        addon: "sample_library_wasm",
        model: "sample_library.loan",
        method: "renew",
        decl: erp_orm::rpc::RpcDecl::Expose {
            effect: erp_orm::rpc::Effect::Write,
            receiver: erp_orm::rpc::Receiver::Records,
            params: &[erp_orm::rpc::ParamSpec::new(
                "note",
                <Option<String> as erp_orm::rpc::FromRpc>::REQUIRED,
                <Option<String> as erp_orm::rpc::FromRpc>::EXPECTED,
            )],
            bind: native_renew_method,
        },
        file: file!(),
        line: line!(),
    },
    erp_orm::rpc::RpcImpl {
        addon: "sample_library_wasm",
        model: "sample_library.lend.wizard",
        method: "action_confirm",
        decl: erp_orm::rpc::RpcDecl::Expose {
            effect: erp_orm::rpc::Effect::Write,
            receiver: erp_orm::rpc::Receiver::Records,
            params: &[],
            bind: |tx, records, _args| native_wizard_method(tx, records, "action_confirm"),
        },
        file: file!(),
        line: line!(),
    },
];

fn native_wizard_method<'a>(
    tx: &'a mut erp_orm::Transaction,
    records: erp_orm::Records,
    method: &'static str,
) -> erp_orm::BoxFuture<'a, Result<erp_fields::Value, erp_orm::rpc::BindError>> {
    Box::pin(async move {
        // Dispatch the real linked implementation with the caller's transaction:
        // no sudo, replacement business logic, or swallowed method error.
        let result =
            erp_orm::call_method::<(), serde_json::Value>(tx, &records, method, ()).await?;
        Ok(erp_fields::Value::Json(Arc::new(result)))
    })
}

fn native_return_method<'a>(
    tx: &'a mut erp_orm::Transaction,
    records: erp_orm::Records,
) -> erp_orm::BoxFuture<'a, Result<erp_fields::Value, erp_orm::rpc::BindError>> {
    Box::pin(async move {
        let result = erp_orm::call_method::<(), bool>(tx, &records, "give_back", ()).await?;
        Ok(erp_orm::rpc::IntoRpc::into_rpc(result)?)
    })
}

fn native_renew_method<'a>(
    tx: &'a mut erp_orm::Transaction,
    records: erp_orm::Records,
    mut args: erp_orm::rpc::RpcArgs,
) -> erp_orm::BoxFuture<'a, Result<erp_fields::Value, erp_orm::rpc::BindError>> {
    Box::pin(async move {
        let note = args.take::<Option<String>>("note")?;
        let result =
            erp_orm::call_method::<(Option<String>,), i32>(tx, &records, "renew", (note,)).await?;
        Ok(erp_orm::rpc::IntoRpc::into_rpc(result)?)
    })
}

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

async fn rpc(
    service: &RpcService,
    principal: &erp_orm::Principal,
    model: &str,
    method: &str,
    args: serde_json::Value,
) -> erp_rpc::RpcResponse {
    service
        .call(RpcCall::json2(
            &format!("/json/2/{model}/{method}"),
            serde_json::to_vec(&args).unwrap(),
            Caller::Principal(principal.clone()),
        ))
        .await
}
async fn ok(
    service: &RpcService,
    principal: &erp_orm::Principal,
    model: &str,
    method: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let response = rpc(service, principal, model, method, args).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    result(&response)
}

#[test]
fn installed_native_lending_wizard_uses_ordinary_acl_and_persists_the_loan() {
    thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(installed_native());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn installed_native() {
    use serde_json::json;
    // Refuse a fixture declaration that no longer matches the addon's explicit
    // package opt-in. This does not infer native exposure for arbitrary methods.
    let description: serde_json::Value = serde_json::from_str(&erp_sdk::describe()).unwrap();
    for declaration in &NATIVE_WIZARD_RPC {
        let contribution = description["contributions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| {
                entry["addon"] == declaration.addon
                    && entry["kind"] == "method"
                    && entry["model"] == declaration.model
                    && entry["name"] == declaration.method
            })
            .expect("native fixture must expose a declared package method");
        assert_eq!(contribution["detail"]["rpc"], true);
        let erp_orm::rpc::RpcDecl::Expose { params, .. } = declaration.decl else {
            panic!("native fixture table must contain explicit exposures");
        };
        assert_eq!(
            contribution["detail"]["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            params.iter().map(|param| param.name()).collect::<Vec<_>>()
        );
    }
    let db = TestDb::create().await.unwrap();
    let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == "target" || entry.file_name() == "node_modules" {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &to.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }
    // Only the private fixture manifest loses [wasm]; the installed code is linked natively.
    let addon = temp.path().join("sample_library_wasm");
    copy(&root.join("examples/wasm/sample_library_wasm"), &addon);
    let manifest = std::fs::read_to_string(addon.join("manifest.toml")).unwrap();
    let (native, _) = manifest
        .split_once("[wasm]")
        .expect("sample component manifest");
    // addon! adds the native base dependency implicitly. Match the linked descriptor
    // in this private manifest; the shipped component manifest remains untouched.
    let mut native: toml::Value = toml::from_str(native).unwrap();
    native["depends"] = toml::Value::Array(
        erp_addon_sample_library_wasm::ADDON
            .depends
            .iter()
            .map(|name| toml::Value::String((*name).to_owned()))
            .collect(),
    );
    std::fs::write(
        addon.join("manifest.toml"),
        toml::to_string(&native).unwrap(),
    )
    .unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(root.join("addons"));
    index.add_dev_path(temp.path());
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
            &erp_addon_sample_library_wasm::ADDON,
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
    assert!(
        initialized.post_commit.is_clean(),
        "{:?}",
        initialized.post_commit
    );
    assert!(initialized.orm.registry().model("res.users").is_some());
    drop(initialized);
    let env = system.system_env(erp_orm::Context::new());
    let installed = system
        .button_immediate_install(&env, &[ModuleName::new("sample_library_wasm").unwrap()])
        .await
        .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    let wizard_model = installed
        .orm
        .registry()
        .model("sample_library.lend.wizard")
        .unwrap();
    assert_eq!(wizard_model.kind, erp_registry::ModelKind::Transient);
    assert!(matches!(
        &wizard_model.field("borrower_id").unwrap().ty,
        erp_fields::FieldType::Many2one { comodel, .. } if comodel == "res.partner"
    ));
    assert_eq!(
        wizard_model.field("due_date").unwrap().ty,
        erp_fields::FieldType::Date
    );
    let source = PolicySource::build(Arc::clone(installed.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    // A newly policy-bound runtime is a fresh lineage: rebuild preserves access config
    // and deliberately refuses replacing its resolver/policy.
    let orm = erp_orm::Orm::builder(Arc::clone(installed.orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(installed.orm.registry()),
            installed
                .orm
                .env(SUPERUSER_ID, erp_orm::Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(installed.orm.registry()).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .rpc_impls(NATIVE_WIZARD_RPC.iter())
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
    let company = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_company' AND model='res.company'",
            &[],
        ).await.unwrap()[0].get::<_, i64>(0);
    let system_group = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system' AND model='res.groups'",
            &[],
        ).await.unwrap()[0].get::<_, i64>(0);
    // As in human_metadata's installed fixture and the native bootstrap command,
    // create an active system administrator first through the ordinary ORM hooks.
    // The last-administrator constraint stays enabled for all subsequent user writes.
    create_user(
        &mut tx,
        "native-admin",
        "native-admin-password",
        company,
        &[system_group],
    )
    .await;
    let group = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_user'",
            &[],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    let portal = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_portal'",
            &[],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    create_user(
        &mut tx,
        "native-reader",
        "native-reader-password",
        company,
        &[group],
    )
    .await;
    create_user(
        &mut tx,
        "native-outsider",
        "native-outsider-password",
        company,
        &[portal],
    )
    .await;
    tx.commit().await.unwrap();
    let admin = principal(&auth, "native-admin", "native-admin-password").await;
    let reader = principal(&auth, "native-reader", "native-reader-password").await;
    let outsider = principal(&auth, "native-outsider", "native-outsider-password").await;
    assert_ne!(
        reader.subject(),
        SUPERUSER_ID,
        "reader must use ordinary ACLs"
    );
    assert_ne!(
        outsider.subject(),
        SUPERUSER_ID,
        "outsider must use ordinary ACLs"
    );
    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
    // The real linked constraint has no package RPC opt-in or native declaration.
    // Exposing all shipped view methods must not expose internal implementations.
    let private = rpc(
        &service,
        &reader,
        "sample_library.loan",
        "check_renewals",
        json!({"ids":[]}),
    )
    .await;
    assert_eq!(private.status(), 403);
    assert_eq!(result(&private)["name"], "erp.exceptions.AccessError");
    assert_eq!(
        result(&private)["message"],
        "Private methods (such as 'sample_library.loan.check_renewals') cannot be called remotely."
    );
    // Exercise every shipped object-button form before starting the browser;
    // get_views must accept the complete installed form's exposure contract.
    for (model, buttons) in [
        ("sample_library.book", ["lend", "give_back"]),
        ("sample_library.loan", ["renew", "give_back"]),
    ] {
        let views = ok(
            &service,
            &reader,
            model,
            "get_views",
            json!({"views":[[null,"form"]],"wire_major":1,"options":{"mobile":false}}),
        )
        .await;
        if let Some(dir) = std::env::var_os("ERP_STATUSBAR_FIXTURE_DIR") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{}.json", model.replace('.', "-"))),
                serde_json::to_vec_pretty(&views).unwrap(),
            )
            .unwrap();
        }
        if model == "sample_library.book" {
            let nodes = views["views"][0]["nodes"].as_array().unwrap();
            assert!(
                nodes
                    .iter()
                    .any(|n| n["kind"] == "stat_button" && n["name"] == "open_loans"),
                "{views}"
            );
            assert!(
                nodes
                    .iter()
                    .any(|n| n["widget"] == "relation_count" && n["field"]["name"] == "loan_ids"),
                "{views}"
            );
        }
        if model == "sample_library.loan" {
            let nodes = views["views"][0]["nodes"].as_array().unwrap();
            let headers: Vec<_> = nodes
                .iter()
                .filter(|node| node["kind"] == "header")
                .collect();
            assert_eq!(headers.len(), 1, "{views}");
            let children = headers[0]["children"].as_array().unwrap();
            let header_nodes: Vec<_> = children
                .iter()
                .map(|index| &nodes[usize::try_from(index.as_u64().unwrap()).unwrap()])
                .collect();
            assert_eq!(header_nodes[0]["widget"], "statusbar");
            assert_eq!(header_nodes[0]["field"]["name"], "state");
            assert_eq!(header_nodes[0]["options"], json!({"visible":null}));
            assert_eq!(header_nodes[1]["name"], "renew");
            assert_eq!(header_nodes[2]["name"], "give_back");
            assert_eq!(views["fields"][model]["state"]["readonly"], false);
            assert_eq!(
                views["fields"][model]["state"]["selection"],
                json!([
                    ["draft", "Draft"],
                    ["out", "On Loan"],
                    ["returned", "Returned"]
                ])
            );
        }
        for button in buttons {
            assert!(
                views.to_string().contains(&format!("\"{button}\"")),
                "{views}"
            );
        }
    }
    let book = ok(
        &service,
        &reader,
        "sample_library.book",
        "create",
        json!({"vals_list":[{"title":"Native Kindred"}]}),
    )
    .await[0]
        .as_i64()
        .unwrap();
    let borrower = ok(
        &service,
        &admin,
        "res.partner",
        "create",
        json!({"vals_list":[{"name":"Native Octavia"}]}),
    )
    .await[0]
        .as_i64()
        .unwrap();
    let loan_defaults = ok(
        &service,
        &reader,
        "sample_library.loan",
        "default_get",
        json!({"fields":["state"]}),
    )
    .await;
    assert_eq!(loan_defaults["state"], "draft");
    let editable_loan = ok(
        &service,
        &reader,
        "sample_library.loan",
        "create",
        json!({"vals_list":[{"book_id":book,"borrower_id":borrower}]}),
    )
    .await[0]
        .as_i64()
        .unwrap();
    ok(
        &service,
        &reader,
        "sample_library.loan",
        "web_save",
        json!({"ids":[editable_loan],"vals":{"state":"returned"},"specification":{"state":{}}}),
    )
    .await;
    let saved_state = ok(
        &service,
        &reader,
        "sample_library.loan",
        "web_read",
        json!({"ids":[editable_loan],"specification":{"state":{}}}),
    )
    .await;
    assert_eq!(saved_state[0]["state"], "returned");
    ok(
        &service,
        &reader,
        "sample_library.loan",
        "unlink",
        json!({"ids":[editable_loan]}),
    )
    .await;

    let action = ok(
        &service,
        &reader,
        "sample_library.book",
        "lend",
        json!({"ids":[book]}),
    )
    .await;
    assert_eq!(action["target"], "new");
    assert_eq!(action["res_model"], "sample_library.lend.wizard");
    let views = ok(
        &service,
        &reader,
        "sample_library.lend.wizard",
        "get_views",
        json!({"views":[[null,"form"]],"wire_major":1,"options":{"mobile":false}}),
    )
    .await;
    for name in ["book_id", "borrower_id", "due_date", "action_confirm"] {
        assert!(
            views.to_string().contains(&format!("\"{name}\"")),
            "{views}"
        );
    }
    let defaults = ok(
        &service,
        &reader,
        "sample_library.lend.wizard",
        "default_get",
        json!({"fields":["book_id","due_date"],"context":action["context"]}),
    )
    .await;
    assert_eq!(defaults["book_id"], book);
    assert_eq!(defaults["due_date"], action["context"]["default_due_date"]);
    let wizard = ok(&service, &reader, "sample_library.lend.wizard", "create",
        json!({"vals_list":[{"book_id":book,"borrower_id":borrower,"due_date":defaults["due_date"]}]})).await[0].as_i64().unwrap();
    let done = ok(
        &service,
        &reader,
        "sample_library.lend.wizard",
        "action_confirm",
        json!({"ids":[wizard]}),
    )
    .await;
    assert_eq!(done["tag"], "display_notification");
    assert_eq!(
        done["params"]["next"]["type"],
        "ir.actions.act_window_close"
    );
    assert_eq!(
        ok(
            &service,
            &reader,
            "sample_library.book",
            "relation_count",
            json!({"ids":[book],"field":"loan_ids"})
        )
        .await,
        json!(1)
    );
    let loans_action = ok(
        &service,
        &reader,
        "sample_library.book",
        "open_loans",
        json!({"ids":[book]}),
    )
    .await;
    assert_eq!(loans_action["target"], "current");
    assert_eq!(loans_action["domain"], json!([["book_id", "=", book]]));
    assert_eq!(loans_action["context"]["default_book_id"], book);
    let visible_loans = ok(
        &service,
        &reader,
        "sample_library.loan",
        "search_count",
        json!({"domain":loans_action["domain"]}),
    )
    .await;
    assert_eq!(visible_loans, json!(1));
    let client = db.client().await.unwrap();
    let row = client
        .query_one(
            "SELECT borrower_id, due_date::text, state FROM sample_library_loan WHERE book_id=$1",
            &[&book],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), borrower);
    assert_eq!(
        row.get::<_, String>(1),
        defaults["due_date"].as_str().unwrap()
    );
    assert_eq!(row.get::<_, String>(2), "out");
    let denied = rpc(
        &service,
        &outsider,
        "sample_library.book",
        "lend",
        json!({"ids":[book]}),
    )
    .await;
    assert_eq!(denied.status(), 403);
    assert_eq!(result(&denied)["name"], "erp.exceptions.AccessError");
    assert_eq!(
        result(&denied)["message"],
        "Your access rights do not let you access 'Library Book' (sample_library.book) records.\n\nGroups that may do this:\n\t- Access Administration\n\t- Internal User\n\t- Regular User\n\t- Settings Administration\n\nAsk an administrator if you need this access."
    );
    ok(
        &service,
        &reader,
        "sample_library.lend.wizard",
        "unlink",
        json!({"ids":[wizard]}),
    )
    .await;
    let missing = rpc(
        &service,
        &reader,
        "sample_library.lend.wizard",
        "action_confirm",
        json!({"ids":[wizard]}),
    )
    .await;
    assert_eq!(missing.status(), 404);
    assert_eq!(result(&missing)["name"], "erp.exceptions.MissingError");
    let reader_id: i64 = client
        .query_one(
            "SELECT id FROM res_users WHERE login=$1",
            &[&"native-reader"],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        result(&missing)["message"],
        format!(
            "Record does not exist or has been deleted.\n(Record: sample_library.lend.wizard({wizard},), User: {reader_id})"
        )
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM sample_library_loan WHERE book_id=$1",
                &[&book]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    if std::env::var_os("ERP_WIZARD_BROWSER_DRIVER").is_some() {
        native_browser(&db, &orm, &reader).await;
    }
    drop(service);
    drop(auth);
    drop(orm);
    drop(installed);
    drop(system);
    drop(client);
    db.cleanup().await.unwrap();
}

#[path = "support/wizard_browser.rs"]
mod wizard_browser;

/// Exercise the exact production HTTP/authentication boundary with the linked native addon.
/// Installation is complete before serving; no lifecycle adapter or refresh claim is made.
async fn native_browser(db: &TestDb, installed: &erp_orm::Orm, reader: &erp_orm::Principal) {
    use erp_testkit::tokio_postgres::config::Host as PgHost;
    use serde_json::json;
    use std::process::Command as Process;

    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let assets = std::env::var_os("ERP_WIZARD_ASSETS")
        .expect("native browser lane requires compiled ERP_WIZARD_ASSETS");
    // AssetBundle itself validates this manifest and all referenced files before serving.
    for entry in std::fs::read_dir(&assets).unwrap() {
        let entry = entry.unwrap();
        assert!(
            entry.file_type().unwrap().is_file(),
            "compiled asset directory must contain files only"
        );
        std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let role = format!("{}_browser", db.name());
    let client = db.client().await.unwrap();
    client
        .batch_execute(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password'",
            erp_sql::quote(&role)
        ))
        .await
        .unwrap();
    fn encoded(bytes: &[u8]) -> String {
        let mut result = String::new();
        for byte in bytes {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
                result.push(char::from(*byte));
            } else {
                result.push_str(&format!("%{byte:02X}"));
            }
        }
        result
    }
    let connection = db.config();
    let PgHost::Tcp(host) = &connection.get_hosts()[0] else {
        panic!("native browser fixture requires the configured TCP test cluster");
    };
    let url = |user: &str, password: &[u8]| {
        format!(
            "postgresql://{}:{}@{}:{}/{}",
            encoded(user.as_bytes()),
            encoded(password),
            host,
            connection.get_ports()[0],
            db.name()
        )
    };
    wizard_browser::private(
        &dir.path().join("owner-url"),
        &url(
            connection.get_user().unwrap(),
            connection.get_password().unwrap(),
        ),
    );
    wizard_browser::private(
        &dir.path().join("runtime-url"),
        &url(&role, b"fixture-runtime-password"),
    );
    let config_path = dir.path().join("server.toml");
    std::fs::write(&config_path, format!(
        "database='{}'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nbind='127.0.0.1:{port}'\npublic_origin='http://127.0.0.1:{port}'\nproxy_mode='direct'\naddons_path=['{}']\nassets_manifest='manifest.json'\n", db.name(), dir.path().display())).unwrap();
    let grants = Process::new(env!("CARGO_BIN_EXE_erp"))
        .args(["db", "publish-grants", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    // This command never prints connection strings; keep its status and ordinary failure text.
    assert!(
        grants.status.success(),
        "{}",
        String::from_utf8_lossy(&grants.stderr)
    );
    login_rollout::prepare(db, &role).await;
    let config = Arc::new(erp_server::config::ServerConfig::load(&config_path).unwrap());
    let mut runtime_connection = db.config();
    runtime_connection
        .user(&role)
        .password("fixture-runtime-password");
    let pool = erp_orm::pool::create_pool(runtime_connection, 8).unwrap();
    let source = PolicySource::build(Arc::clone(installed.registry()), pool.clone())
        .await
        .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let orm = erp_orm::Orm::builder(Arc::clone(installed.registry()), pool.clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(installed.registry()),
            installed
                .env(SUPERUSER_ID, erp_orm::Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(installed.registry()).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .rpc_impls(NATIVE_WIZARD_RPC.iter())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let auth = Arc::new(
        Auth::new(
            orm.clone(),
            erp_security::auth::AuthConfig::DEFAULT,
            HashPool::global(),
        )
        .await
        .unwrap(),
    );
    // Fixture contact creation requires the installed Settings Administration ACL;
    // all wizard behavior below is still exercised by native-reader.
    let admin = principal(&auth, "native-admin", "native-admin-password").await;
    let service = Arc::new(RpcService::new(orm, Host, RpcConfig::default()));
    let book = ok(
        &service,
        reader,
        "sample_library.book",
        "create",
        json!({"vals_list":[{"title":"Browser Kindred"}]}),
    )
    .await[0]
        .as_i64()
        .unwrap();
    let borrower = ok(
        &service,
        &admin,
        "res.partner",
        "create",
        json!({"vals_list":[{"name":"Octavia"}]}),
    )
    .await[0]
        .as_i64()
        .unwrap();
    let app = Arc::new(
        erp_server::web::WebApp::new(config, auth)
            .unwrap()
            .with_rpc(service),
    );
    // Isolated wizard/authorization fixture; full native startup tests own GC supervision.
    app.set_login_available(true);
    let serving = tokio::spawn(app.serve(listener));
    // Aborting an enclosing test must also stop its owned listener and connection tasks.
    struct Serving(tokio::task::JoinHandle<std::io::Result<()>>);
    impl Drop for Serving {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut serving = Serving(serving);
    let contract = json!({
        "fixtureKind":"native", "schema":1,"url":format!("http://127.0.0.1:{port}"),"database":db.name(),
        "login":"native-reader","password":"native-reader-password",
        "deniedLogin":"native-outsider","deniedPassword":"native-outsider-password",
        "bookModel":"sample_library.book","wizardModel":"sample_library.lend.wizard","loanModel":"sample_library.loan",
        "bookId":book,"bookTitle":"Browser Kindred","borrowerId":borrower,"borrowerName":"Octavia",
        "openMethod":"lend","confirmMethod":"action_confirm",
        "deniedError":{"status":403,"name":"erp.exceptions.AccessError","message":"Your access rights do not let you access 'Library Book' (sample_library.book) records.\n\nGroups that may do this:\n\t- Access Administration\n\t- Internal User\n\t- Regular User\n\t- Settings Administration\n\nAsk an administrator if you need this access."}
    });
    let browser =
        tokio::task::spawn_blocking(move || wizard_browser::run(dir.path(), contract)).await;
    let exited = serving.0.is_finished();
    serving.0.abort();
    match (&mut serving.0).await {
        Err(error) if error.is_cancelled() => {}
        Err(error) => panic!("native browser server task failed: {error}"),
        Ok(result) => panic!("native browser server stopped unexpectedly: {result:?}"),
    }
    pool.close();
    client
        .batch_execute(&format!(
            "DROP OWNED BY {} CASCADE; DROP ROLE {}",
            erp_sql::quote(&login_rollout::role(&role)),
            erp_sql::quote(&login_rollout::role(&role))
        ))
        .await
        .unwrap();
    client
        .batch_execute(&format!(
            "DROP OWNED BY {}; DROP ROLE {}",
            erp_sql::quote(&role),
            erp_sql::quote(&role)
        ))
        .await
        .unwrap();
    browser.unwrap();
    assert!(
        !exited,
        "native browser server stopped before fixture teardown"
    );
}
