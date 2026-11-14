// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fiscal positions on a real module installation and its stored access policy.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_sdk::{Context, ErrorKind, Orm, SUPERUSER_ID, prelude::*};
use erp_testkit::TestDb;
#[path = "support/fpos.rs"]
mod support;
use std::path::{Path, PathBuf};
fn addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../addons")
        .canonicalize()
        .unwrap()
}
async fn install() -> (TestDb, Orm) {
    let db = TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons());
    let identities = index
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
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_analytic::ADDON,
            &erp_addon_account::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let initialized = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = initialized.orm.env(SUPERUSER_ID, Context::new());
    let outcome =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("account").unwrap()]))
            .await
            .unwrap();
    (db, outcome.orm)
}
fn run<F: std::future::Future<Output = ()> + 'static>(work: fn() -> F) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(work())
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn xid(tx: &mut Transaction, module: &str, name: &str) -> i64 {
    tx.query(
        "SELECT res_id::int8 FROM ir_model_data WHERE module=$1 AND name=$2",
        &[&module, &name],
    )
    .await
    .unwrap()[0]
        .get(0)
}
fn text(s: &str) -> Cell {
    Cell::Str(s.into())
}
#[test]
fn installed_fiscal_position_models_defaults_and_permissions() {
    run(installed);
}
async fn installed() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let company = xid(&mut tx, "base", "main_company").await;
    let country = xid(&mut tx, "base", "fr").await;
    assert!(orm.registry().model_id("account.fiscal.position").is_some());
    assert!(
        orm.registry()
            .model_id("account.fiscal.position.account")
            .is_some()
    );
    let empty: i64 = tx
        .query("SELECT count(*) FROM account_fiscal_position", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        empty, 0,
        "no universal company tax rule is seeded before a chart is chosen"
    );
    let mappings: i64 = tx
        .query("SELECT count(*) FROM account_fiscal_position_account", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(mappings, 0);
    let vat_group = xid(&mut tx, "account", "europe_vat").await;
    let group = tx
        .env()
        .records("res.country.group")
        .unwrap()
        .browse([vat_group])
        .unwrap();
    assert_eq!(group.cell(&mut tx, "code").await.unwrap(), text("EU-VAT"));
    assert_eq!(
        group
            .related(&mut tx, "exclude_state_ids")
            .await
            .unwrap()
            .len(),
        7
    );
    let intrastat = xid(&mut tx, "account", "intrastat").await;
    assert_eq!(
        tx.env()
            .records("res.country.group")
            .unwrap()
            .browse([intrastat])
            .unwrap()
            .cell(&mut tx, "code")
            .await
            .unwrap(),
        text("INTRASTAT")
    );
    let revision: i32 = tx
        .query(
            "SELECT fpos_revision FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(revision, 0);
    let env = tx.env().with_company(company).unwrap();
    let model = env.records("account.fiscal.position").unwrap();
    let fp = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Install foreign rule"))
                    .set_cell("country_id", Cell::Id(country))
                    .set_cell("foreign_vat", text(" fr.1234 ")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        fp.cell(&mut tx, "foreign_vat").await.unwrap(),
        text("FR1234")
    );
    let manager = xid(&mut tx, "account", "group_account_manager").await;
    let reader = xid(&mut tx, "base", "group_user").await;
    let system = xid(&mut tx, "base", "group_system").await;
    let users = env.records("res.users").unwrap();
    let mut ids = Vec::new();
    for (name, group) in [
        ("fpos-admin", system),
        ("fpos-manager", manager),
        ("fpos-reader", reader),
    ] {
        let user = users
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text(name))
                        .set_cell("login", text(name))
                        .set_cell("company_id", Cell::Id(company))
                        .set_commands(
                            "company_ids",
                            [Command::set([RecId::from_real(company).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::set([RecId::from_real(group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        ids.push(user.ids()[0]);
    }
    let germany = xid(&mut tx, "base", "de").await;
    let hidden = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Hidden VAT identity"))
                    .set_cell("country_id", Cell::Id(germany))
                    .set_cell("foreign_vat", text("DE1234")),
            ],
        )
        .await
        .unwrap();
    let model_id = xid(&mut tx, "account", "model_account_fiscal_position").await;
    env.records("ir.access")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Fiscal-position redaction fixture"))
                    .set_cell("model_id", Cell::Id(model_id))
                    .set_cell("operation", text("r"))
                    .set_cell("domain", text("[('name','!=','Hidden VAT identity')]")),
            ],
        )
        .await
        .unwrap();
    let branch = env
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Branch fixture"))
                    .set_cell("parent_id", Cell::Id(company)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let child_user = users
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("fpos-child"))
                    .set_cell("login", text("fpos-child"))
                    .set_cell("company_id", Cell::Id(branch))
                    .set_commands(
                        "company_ids",
                        [Command::set([RecId::from_real(branch).unwrap()])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::set([RecId::from_real(manager).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    fp.write(
        &mut tx,
        RawVals::new().set_cell("auto_apply", Cell::Bool(true)),
    )
    .await
    .unwrap();
    let delivery = env
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("French delivery"))
                    .set_cell("country_id", Cell::Id(country)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    // A sibling branch and records the branch user cannot see; a rule the parent user cannot see.
    let sibling = env
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Sibling fixture"))
                    .set_cell("parent_id", Cell::Id(company)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let sibling_partner = env
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Sibling customer"))
                    .set_cell("country_id", Cell::Id(country))
                    .set_cell("company_id", Cell::Id(sibling)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let sibling_tax = support::tax(&mut tx, sibling, country, "Sibling tax")
        .await
        .ids()[0];
    // Account codes live on the root company, so the unreadable account belongs to another root.
    let currency: i64 = tx
        .query(
            "SELECT currency_id::int8 FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()[0]
        .get(0);
    let other_root = env
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Other root fixture"))
                    .set_cell("currency_id", Cell::Id(currency)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let hidden_account = support::model(&tx, other_root, "account.account")
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Sibling income"))
                    .set_cell("code", text("4999"))
                    .set_cell("account_type", text("income")),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let branch_rule = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Branch rule"))
                    .set_cell("company_id", Cell::Id(branch)),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let source =
        erp_security::source::PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
            .await
            .unwrap();
    let secured = Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .resolver(erp_security::resolver::Resolver::for_registry(orm.registry()).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let mut read = secured
        .begin(erp_sdk::UserId::new(ids[2]).unwrap())
        .await
        .unwrap();
    let rec = read
        .env()
        .with_company(company)
        .unwrap()
        .records("account.fiscal.position")
        .unwrap()
        .browse(fp.ids())
        .unwrap();
    assert_eq!(
        rec.cell(&mut read, "foreign_vat").await.unwrap(),
        text("FR1234")
    );
    let error = rec
        .write(&mut read, RawVals::new().set_cell("name", text("Refused")))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you modify 'Fiscal position' (account.fiscal.position) records.\n\nGroups that may do this:\n\t- Accounting Administrator\n\t- Settings Administration\n\nAsk an administrator if you need this access."
    );
    read.rollback().await.unwrap();
    drop(read);
    let mut manage = secured
        .begin(erp_sdk::UserId::new(ids[1]).unwrap())
        .await
        .unwrap();
    let rec = manage
        .env()
        .with_company(company)
        .unwrap()
        .records("account.fiscal.position")
        .unwrap()
        .browse(fp.ids())
        .unwrap();
    rec.write(
        &mut manage,
        RawVals::new().set_cell("name", text("Manager edited")),
    )
    .await
    .unwrap();
    assert_eq!(
        rec.cell(&mut manage, "name").await.unwrap(),
        text("Manager edited")
    );
    let owner = manage
        .env()
        .with_company(company)
        .unwrap()
        .records("res.company")
        .unwrap()
        .browse([company])
        .unwrap();
    let error = owner.cell(&mut manage, "fpos_revision").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"fpos_revision\" on Company (res.company). Please contact your system administrator.\n\nOperation: read"
    );
    let error = manage
        .env()
        .with_company(company)
        .unwrap()
        .records("account.fiscal.position")
        .unwrap()
        .browse(hidden.ids())
        .unwrap()
        .check_access(&mut manage, erp_sdk::Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        format!(
            "Access rules protect some of these records.\n\nfpos-manager (id={}) has no 'read' access to:\n- Fiscal position (account.fiscal.position)\n\nAsk an administrator if you need access to these records.",
            ids[1]
        )
    );
    let proposer = manage
        .env()
        .with_company(company)
        .unwrap()
        .records("account.fiscal.position")
        .unwrap();
    // A refused mutation poisons its transaction; the savepoint keeps the session usable.
    let error = manage
        .savepoint(async |tx| {
            proposer
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text("Visible proposed"))
                            .set_cell("country_id", Cell::Id(germany))
                            .set_cell("foreign_vat", text("DE5678")),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Another fiscal position in the company tree already uses a different foreign_vat for this country."
    );
    // A parent-only manager cannot reach down into a branch: no create, move or read there.
    let positions = manage
        .env()
        .with_company(company)
        .unwrap()
        .records("account.fiscal.position")
        .unwrap();
    let error = manage
        .savepoint(async |tx| {
            positions
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text("Parent user branch rule"))
                            .set_cell("company_id", Cell::Id(branch)),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    outside_tree(&error);
    let error = manage
        .savepoint(async |tx| {
            positions
                .browse(fp.ids())
                .unwrap()
                .write(tx, RawVals::new().set_cell("company_id", Cell::Id(branch)))
                .await
        })
        .await
        .unwrap_err();
    outside_tree(&error);
    let error = positions
        .browse(branch_rule.ids())
        .unwrap()
        .check_access(&mut manage, erp_sdk::Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    manage.rollback().await.unwrap();
    drop(manage);
    let mut child = secured
        .begin(erp_sdk::UserId::new(child_user).unwrap())
        .await
        .unwrap();
    let selected: Option<i64> = child
        .env()
        .with_company(branch)
        .unwrap()
        .records("account.fiscal.position")
        .unwrap()
        .call(&mut child, "_get_fiscal_position", (delivery, None::<i64>))
        .await
        .unwrap();
    assert_eq!(selected, Some(fp.ids()[0]));
    child
        .env()
        .with_company(branch)
        .unwrap()
        .records("account.fiscal.position")
        .unwrap()
        .browse(fp.ids())
        .unwrap()
        .write(
            &mut child,
            RawVals::new().set_cell("name", text("Branch manager edited parent rule")),
        )
        .await
        .unwrap();
    // A branch manager cannot place a rule in a sibling branch, nor map or resolve with
    // records of companies outside its tree.
    let positions = child
        .env()
        .with_company(branch)
        .unwrap()
        .records("account.fiscal.position")
        .unwrap();
    let error = child
        .savepoint(async |tx| {
            positions
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text("Branch user sibling rule"))
                            .set_cell("company_id", Cell::Id(sibling)),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    outside_tree(&error);
    let error = child
        .savepoint(async |tx| {
            positions
                .browse(fp.ids())
                .unwrap()
                .write(tx, RawVals::new().set_cell("company_id", Cell::Id(sibling)))
                .await
        })
        .await
        .unwrap_err();
    outside_tree(&error);
    let rule = positions.browse(fp.ids()).unwrap();
    let error = rule
        .call::<_, Vec<i64>>(&mut child, "map_tax", (vec![sibling_tax],))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    let error = rule
        .call::<_, i64>(&mut child, "map_account", (hidden_account,))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    let error = positions
        .call::<_, Option<i64>>(
            &mut child,
            "_get_fiscal_position",
            (sibling_partner, None::<i64>),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    child.rollback().await.unwrap();
    drop(child);
    // A settings administrator (not superuser mode) can neither write nor search the fence.
    let mut admin = secured
        .begin(erp_sdk::UserId::new(ids[0]).unwrap())
        .await
        .unwrap();
    let companies = admin
        .env()
        .with_company(company)
        .unwrap()
        .records("res.company")
        .unwrap();
    let error = admin
        .savepoint(async |tx| {
            companies
                .browse([company])
                .unwrap()
                .write(tx, RawVals::new().set_cell("fpos_revision", Cell::Int(7)))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"fpos_revision\" on Company (res.company). Please contact your system administrator.\n\nOperation: write"
    );
    let error = companies
        .search(&mut admin, &Domain::field("fpos_revision").unwrap().eq(0))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"fpos_revision\" on Company (res.company). Please contact your system administrator.\n\nOperation: read"
    );
    admin.rollback().await.unwrap();
    drop(admin);
    drop(secured);
    // Superuser mode skips field groups; the account company override still refuses the
    // fence column by name, while the fence's own increment keeps working.
    let mut su = orm.begin(SUPERUSER_ID).await.unwrap();
    let revision = async |tx: &mut Transaction| -> i32 {
        tx.query(
            "SELECT fpos_revision FROM res_company WHERE id=$1",
            &[&company],
        )
        .await
        .unwrap()[0]
            .get(0)
    };
    let before = revision(&mut su).await;
    su.env()
        .with_company(company)
        .unwrap()
        .records("account.fiscal.position")
        .unwrap()
        .create(
            &mut su,
            vec![RawVals::new().set_cell("name", text("Fence step"))],
        )
        .await
        .unwrap();
    assert_eq!(revision(&mut su).await, before + 1);
    let companies = su
        .env()
        .with_company(company)
        .unwrap()
        .records("res.company")
        .unwrap();
    let fence_refused = |error: Error| {
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "fpos_revision is internal to fiscal position writes and cannot be set."
        );
    };
    fence_refused(
        su.savepoint(async |tx| {
            companies
                .browse([company])
                .unwrap()
                .write(
                    tx,
                    RawVals::new().set_cell("fpos_revision", Cell::Int(i32::MAX)),
                )
                .await
        })
        .await
        .unwrap_err(),
    );
    fence_refused(
        su.savepoint(async |tx| {
            companies
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text("Fence supplied"))
                            .set_cell("parent_id", Cell::Id(company))
                            .set_cell("fpos_revision", Cell::Int(i32::MAX)),
                    ],
                )
                .await
        })
        .await
        .unwrap_err(),
    );
    let defaulted =
        companies.with_context([("default_fpos_revision", Value::Int(i64::from(i32::MAX)))]);
    fence_refused(
        su.savepoint(async |tx| {
            defaulted
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text("Fence defaulted"))
                            .set_cell("parent_id", Cell::Id(company)),
                    ],
                )
                .await
        })
        .await
        .unwrap_err(),
    );
    assert_eq!(revision(&mut su).await, before + 1);
    su.rollback().await.unwrap();
    drop(su);
    drop(orm);
    db.cleanup().await.unwrap();
}
#[track_caller]
fn outside_tree(error: &Error) {
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Access,
            "The fiscal position company is outside the active company hierarchy."
        )
    );
}
