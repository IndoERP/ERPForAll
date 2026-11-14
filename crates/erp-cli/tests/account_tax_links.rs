// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tax foundation on a genuine account installation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_sdk::prelude::*;
use erp_sdk::{Context, ErrorKind, Orm, SUPERUSER_ID};
use erp_testkit::TestDb;
use std::path::{Path, PathBuf};
fn big_stack<F: std::future::Future<Output = ()> + 'static>(test: fn() -> F) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(test());
        })
        .unwrap()
        .join()
        .unwrap();
}

fn addons_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

/// A fresh database with `account` installed.
async fn install() -> (TestDb, Orm) {
    install_modules(&["account", "web"]).await
}
async fn install_modules(requested: &[&str]) -> (TestDb, Orm) {
    let db = TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons_dir());
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
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_analytic::ADDON,
            &erp_addon_account::ADDON,
            &erp_addon_web::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let outcome = Box::pin(
        system.button_immediate_install(
            &env,
            &requested
                .iter()
                .map(|name| ModuleName::new(*name).unwrap())
                .collect::<Vec<_>>(),
        ),
    )
    .await
    .unwrap();
    (db, outcome.orm)
}

async fn id_of(tx: &mut Transaction, sql: &str) -> i64 {
    tx.query(sql, &[]).await.unwrap()[0].get(0)
}

async fn main_company(tx: &mut Transaction) -> i64 {
    id_of(
        tx,
        "SELECT res_id::int8 FROM ir_model_data WHERE module = 'base' AND name = 'main_company'",
    )
    .await
}

fn text(s: &str) -> Cell {
    Cell::Str(s.into())
}
async fn fixture(tx: &mut Transaction) -> (Records, i64) {
    let company = main_company(tx).await;
    let env = tx.env().with_company(company).unwrap();
    let country: i64 = tx
        .query("SELECT id FROM res_country WHERE code='US'", &[])
        .await
        .unwrap()[0]
        .get(0);
    env.records("res.company")
        .unwrap()
        .browse([company])
        .unwrap()
        .write(
            tx,
            RawVals::new().set_cell("account_fiscal_country_id", Cell::Id(country)),
        )
        .await
        .unwrap();
    env.records("account.tax.group")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", text("Tax link group"))],
        )
        .await
        .unwrap();
    (env.records("account.tax").unwrap(), company)
}
async fn tax(taxes: &Records, tx: &mut Transaction, name: &str, role: &str) -> Records {
    taxes
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(name))
                    .set_cell("type_tax_use", text(role)),
            ],
        )
        .await
        .unwrap()
}
async fn account(env: &Env, tx: &mut Transaction, name: &str, kind: &str) -> Records {
    env.records("account.account")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(name))
                    .set_cell("account_type", text(kind)),
            ],
        )
        .await
        .unwrap()
}
async fn write_error(tx: &mut Transaction, record: &Records, vals: RawVals) -> erp_sdk::Error {
    let record = record.clone();
    tx.savepoint(async move |tx| record.write(tx, vals).await)
        .await
        .unwrap_err()
}
fn refused(error: &erp_sdk::Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), message);
}

#[test]
fn account_default_taxes_refuse_off_balance_mutations() {
    big_stack(account_links);
}
async fn account_links() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, _) = fixture(&mut tx).await;
    let sale = tax(&taxes, &mut tx, "Sale link", "sale").await;
    let a = account(taxes.env(), &mut tx, "Taxed", "asset_current").await;
    a.write(
        &mut tx,
        RawVals::new().set_commands(
            "tax_ids",
            [Command::link(RecId::from_real(sale.ids()[0]).unwrap())],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        a.related(&mut tx, "tax_ids").await.unwrap().ids(),
        sale.ids()
    );
    let err = write_error(
        &mut tx,
        &a,
        RawVals::new().set_cell("account_type", text("off_balance")),
    )
    .await;
    refused(
        &err,
        "Off-balance accounts cannot carry default taxes: clear the taxes before changing the account type.",
    );
    assert_eq!(
        a.cell(&mut tx, "account_type").await.unwrap(),
        text("asset_current")
    );
    let off = account(taxes.env(), &mut tx, "Off", "off_balance").await;
    let err = write_error(
        &mut tx,
        &off,
        RawVals::new().set_commands(
            "tax_ids",
            [Command::link(RecId::from_real(sale.ids()[0]).unwrap())],
        ),
    )
    .await;
    refused(
        &err,
        "Off-balance accounts cannot carry default taxes: clear the taxes before changing the account type.",
    );
    assert!(off.related(&mut tx, "tax_ids").await.unwrap().is_empty());
    a.write(
        &mut tx,
        RawVals::new()
            .set_commands("tax_ids", [Command::clear()])
            .set_cell("account_type", text("off_balance")),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn repartition_reference_blocks_account_delete_but_allows_archive() {
    big_stack(account_delete);
}
async fn account_delete() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, _) = fixture(&mut tx).await;
    let t = tax(&taxes, &mut tx, "Distribution", "sale").await;
    let a = account(
        taxes.env(),
        &mut tx,
        "Distribution account",
        "liability_current",
    )
    .await;
    let rows = t.related(&mut tx, "repartition_line_ids").await.unwrap();
    rows.write(
        &mut tx,
        RawVals::new().set_cell("account_id", Cell::Id(a.ids()[0])),
    )
    .await
    .unwrap();
    let target = a.clone();
    let err = tx
        .savepoint(async move |tx| target.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "Tax distributions still use these accounts: Distribution account. Remove their assignments before deleting the accounts."
    );
    a.write(
        &mut tx,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    assert_eq!(a.cell(&mut tx, "active").await.unwrap(), Cell::Bool(false));
    rows.write(&mut tx, RawVals::new().set_cell("account_id", Cell::Null))
        .await
        .unwrap();
    a.unlink(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn company_sale_and_purchase_default_taxes_persist_independently() {
    big_stack(company_defaults);
}
async fn company_defaults() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, c) = fixture(&mut tx).await;
    let sale = tax(&taxes, &mut tx, "Sale default", "sale").await;
    let purchase = tax(&taxes, &mut tx, "Purchase default", "purchase").await;
    let company = taxes
        .env()
        .records("res.company")
        .unwrap()
        .browse([c])
        .unwrap();
    company
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("account_sale_tax_id", Cell::Id(sale.ids()[0]))
                .set_cell("account_purchase_tax_id", Cell::Id(purchase.ids()[0])),
        )
        .await
        .unwrap();
    assert_eq!(
        company.cell(&mut tx, "account_sale_tax_id").await.unwrap(),
        Cell::Id(sale.ids()[0])
    );
    assert_eq!(
        company
            .cell(&mut tx, "account_purchase_tax_id")
            .await
            .unwrap(),
        Cell::Id(purchase.ids()[0])
    );
    company
        .write(
            &mut tx,
            RawVals::new().set_cell("account_sale_tax_id", Cell::Null),
        )
        .await
        .unwrap();
    assert_eq!(
        company.cell(&mut tx, "account_sale_tax_id").await.unwrap(),
        Cell::Null
    );
    assert_eq!(
        company
            .cell(&mut tx, "account_purchase_tax_id")
            .await
            .unwrap(),
        Cell::Id(purchase.ids()[0])
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

async fn company(tx: &mut Transaction, env: &Env, name: &str, parent: Option<i64>) -> i64 {
    let root = main_company(tx).await;
    let currency = env
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap()
        .cell(tx, "currency_id")
        .await
        .unwrap();
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("currency_id", currency);
    if let Some(parent) = parent {
        vals.insert("parent_id".into(), Cell::Id(parent));
    }
    env.records("res.company")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}
async fn defaults_on(tx: &mut Transaction, env: &Env, company: i64, sale: i64, purchase: i64) {
    env.records("res.company")
        .unwrap()
        .browse([company])
        .unwrap()
        .write(
            tx,
            RawVals::new()
                .set_cell("account_sale_tax_id", Cell::Id(sale))
                .set_cell("account_purchase_tax_id", Cell::Id(purchase)),
        )
        .await
        .unwrap();
}
async fn product(tx: &mut Transaction, env: &Env, vals: RawVals) -> Records {
    env.records("product.template")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
}
#[test]
fn product_tax_defaults_preserve_explicit_empty_and_delegate_to_variants() {
    big_stack(product_defaults);
}
async fn product_defaults() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, c) = fixture(&mut tx).await;
    let env = taxes.env().clone();
    let sale = tax(&taxes, &mut tx, "Product sale", "sale").await;
    let purchase = tax(&taxes, &mut tx, "Product purchase", "purchase").await;
    defaults_on(&mut tx, &env, c, sale.ids()[0], purchase.ids()[0]).await;
    let p = product(
        &mut tx,
        &env,
        RawVals::new().set_cell("name", text("Default product")),
    )
    .await;
    assert_eq!(
        p.related(&mut tx, "taxes_id").await.unwrap().ids(),
        sale.ids()
    );
    assert_eq!(
        p.related(&mut tx, "supplier_taxes_id").await.unwrap().ids(),
        purchase.ids()
    );
    let variants = p.related(&mut tx, "product_variant_ids").await.unwrap();
    assert_eq!(
        variants.related(&mut tx, "taxes_id").await.unwrap().ids(),
        sale.ids()
    );
    let empty = product(
        &mut tx,
        &env,
        RawVals::new()
            .set_cell("name", text("Explicit empty"))
            .set_commands("taxes_id", [Command::clear()])
            .set_commands("supplier_taxes_id", [Command::clear()]),
    )
    .await;
    assert!(empty.related(&mut tx, "taxes_id").await.unwrap().is_empty());
    assert!(
        empty
            .related(&mut tx, "supplier_taxes_id")
            .await
            .unwrap()
            .is_empty()
    );
    let model = env.records("product.template").unwrap();
    let defaults = model
        .default_get(&mut tx, &["taxes_id", "supplier_taxes_id"])
        .await
        .unwrap();
    assert_eq!(defaults.len(), 2);
    let draft = model
        .vals_from_values(&defaults)
        .unwrap()
        .set_cell("name", text("Draft defaults"));
    let from_draft = product(&mut tx, &env, draft).await;
    assert_eq!(
        from_draft.related(&mut tx, "taxes_id").await.unwrap().ids(),
        sale.ids()
    );
    let branch = company(&mut tx, &env, "Default branch", Some(c)).await;
    let branch_env = env.with_context([("allowed_company_ids", Value::list([Value::Int(branch)]))]);
    let bp = product(
        &mut tx,
        &branch_env,
        RawVals::new().set_cell("name", text("Root fallback")),
    )
    .await;
    assert_eq!(
        bp.related(&mut tx, "taxes_id").await.unwrap().ids(),
        sale.ids()
    );
    assert_eq!(
        bp.related(&mut tx, "supplier_taxes_id")
            .await
            .unwrap()
            .ids(),
        purchase.ids()
    );
    p.write(&mut tx, RawVals::new().set_cell("type", text("combo")))
        .await
        .unwrap();
    assert!(p.related(&mut tx, "taxes_id").await.unwrap().is_empty());
    assert!(
        p.related(&mut tx, "supplier_taxes_id")
            .await
            .unwrap()
            .is_empty()
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn product_defaults_fallback_only_when_selected_collection_is_empty() {
    big_stack(product_mixed);
}
async fn product_mixed() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, root) = fixture(&mut tx).await;
    let env = taxes.env().clone();
    let root_sale = tax(&taxes, &mut tx, "Root sale", "sale").await;
    let root_purchase = tax(&taxes, &mut tx, "Root purchase", "purchase").await;
    defaults_on(
        &mut tx,
        &env,
        root,
        root_sale.ids()[0],
        root_purchase.ids()[0],
    )
    .await;
    let a = company(&mut tx, &env, "Branch A", Some(root)).await;
    let b = company(&mut tx, &env, "Branch B", Some(root)).await;
    let country = taxes
        .browse(root_sale.ids())
        .unwrap()
        .cell(&mut tx, "country_id")
        .await
        .unwrap();
    let group_id = root_sale.cell(&mut tx, "tax_group_id").await.unwrap();
    let branch_tax = env
        .with_company(a)
        .unwrap()
        .records("account.tax")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Branch sale"))
                    .set_cell("country_id", country)
                    .set_cell("tax_group_id", group_id),
            ],
        )
        .await
        .unwrap();
    env.records("res.company")
        .unwrap()
        .browse([a])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("account_sale_tax_id", Cell::Id(branch_tax.ids()[0])),
        )
        .await
        .unwrap();
    let both = env.with_context([(
        "allowed_company_ids",
        Value::list([Value::Int(a), Value::Int(b)]),
    )]);
    let p = product(
        &mut tx,
        &both,
        RawVals::new().set_cell("name", text("Mixed defaults")),
    )
    .await;
    assert_eq!(
        p.related(&mut tx, "taxes_id").await.unwrap().ids(),
        branch_tax.ids()
    );
    assert_eq!(
        p.related(&mut tx, "supplier_taxes_id").await.unwrap().ids(),
        root_purchase.ids()
    );
    let b_only = env.with_context([("allowed_company_ids", Value::list([Value::Int(b)]))]);
    let other = product(
        &mut tx,
        &b_only,
        RawVals::new().set_cell("name", text("Other selection")),
    )
    .await;
    assert_eq!(
        other.related(&mut tx, "taxes_id").await.unwrap().ids(),
        root_sale.ids()
    );
    p.with_env(b_only)
        .write(
            &mut tx,
            RawVals::new().set_cell("name", text("Keep chosen taxes")),
        )
        .await
        .unwrap();
    assert_eq!(
        p.related(&mut tx, "taxes_id").await.unwrap().ids(),
        branch_tax.ids()
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

async fn user(tx: &mut Transaction, env: &Env, name: &str, companies: &[i64]) -> i64 {
    if tx
        .query(
            "SELECT id FROM res_users WHERE login = 'tax-link-administrator'",
            &[],
        )
        .await
        .unwrap()
        .is_empty()
    {
        let admin: i64 = tx
            .query(
                "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        env.records("res.users")
            .unwrap()
            .create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Tax link administrator"))
                        .set_cell("login", text("tax-link-administrator"))
                        .set_cell("company_id", Cell::Id(companies[0]))
                        .set_commands(
                            "company_ids",
                            [Command::set(
                                companies.iter().map(|id| RecId::from_real(*id).unwrap()),
                            )],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::set([RecId::from_real(admin).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
    }
    let group: i64=tx.query("SELECT res_id FROM ir_model_data WHERE module='product' AND name='group_product_manager'",&[]).await.unwrap()[0].get(0);
    env.records("res.users")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(name))
                    .set_cell("login", text(name))
                    .set_cell("company_id", Cell::Id(companies[0]))
                    .set_commands(
                        "company_ids",
                        [Command::set(
                            companies.iter().map(|id| RecId::from_real(*id).unwrap()),
                        )],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::set([RecId::from_real(group).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}
async fn secure(db: &TestDb, orm: &Orm) -> Orm {
    let source =
        erp_security::source::PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
            .await
            .unwrap();
    Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(orm.registry()),
            orm.env(SUPERUSER_ID, Context::new())
                .generation_attachment::<erp_sdk::views::PublishedViewTypes>()
                .unwrap(),
        ))
        .resolver(erp_security::resolver::Resolver::for_registry(orm.registry()).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}
struct RpcHost;
impl erp_rpc::RpcHost for RpcHost {
    fn language_active(&self, code: &str) -> erp_sdk::Result<bool> {
        Ok(code == "en_US")
    }
    fn has_capability(
        &self,
        _: &erp_orm::Principal,
        _: &erp_orm::Capability,
    ) -> erp_sdk::Result<bool> {
        Ok(false)
    }
}
async fn other_taxes(tx: &mut Transaction, env: &Env, c: i64, country: Cell) -> (Records, Records) {
    let env = env.with_context([("allowed_company_ids", Value::list([Value::Int(c)]))]);
    env.records("res.company")
        .unwrap()
        .browse([c])
        .unwrap()
        .write(
            tx,
            RawVals::new().set_cell("account_fiscal_country_id", country),
        )
        .await
        .unwrap();
    env.records("account.tax.group")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", text("Other group"))],
        )
        .await
        .unwrap();
    let taxes = env.records("account.tax").unwrap();
    let sale = tax(&taxes, tx, "Other sale", "sale").await;
    let purchase = tax(&taxes, tx, "Other purchase", "purchase").await;
    (sale, purchase)
}

const SALE_LABEL: &str = "Taxes charged on sales";
const PURCHASE_LABEL: &str = "Taxes paid on purchases";
/// The admission's refusal of a readable tax: its name and the field's label, no id.
async fn link_refusal(tx: &mut Transaction, tax: &Records, label: &str) -> String {
    let Cell::Str(name) = tax.cell(tx, "display_name").await.unwrap() else {
        panic!("a tax without a display name");
    };
    format!(
        "The tax \"{name}\" cannot be chosen in {label}: it belongs to a company outside this product's companies and their parent companies."
    )
}

#[test]
fn product_tax_links_enforce_caller_and_product_company_on_both_fields() {
    big_stack(product_admission);
}
async fn product_admission() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, a) = fixture(&mut setup).await;
    let env = taxes.env().clone();
    let own_sale = tax(&taxes, &mut setup, "Own sale", "sale").await;
    let own_purchase = tax(&taxes, &mut setup, "Own purchase", "purchase").await;
    let b = company(&mut setup, &env, "Other company", None).await;
    let country = own_sale.cell(&mut setup, "country_id").await.unwrap();
    let (foreign_sale, foreign_purchase) = other_taxes(&mut setup, &env, b, country).await;
    let editor = user(&mut setup, &env, "tax-link-editor", &[a]).await;
    let broad = env.with_context([(
        "allowed_company_ids",
        Value::list([Value::Int(a), Value::Int(b)]),
    )]);
    let p = product(
        &mut setup,
        &env,
        RawVals::new()
            .set_cell("name", text("Own-company product"))
            .set_cell("company_id", Cell::Id(a)),
    )
    .await;
    for (field, label, tax) in [
        ("taxes_id", SALE_LABEL, &foreign_sale),
        ("supplier_taxes_id", PURCHASE_LABEL, &foreign_purchase),
    ] {
        let expected = link_refusal(&mut setup, tax, label).await;
        let vals = RawVals::new().set_commands(
            field.to_owned(),
            [Command::link(RecId::from_real(tax.ids()[0]).unwrap())],
        );
        let target = p.with_env(broad.clone());
        let error = write_error(&mut setup, &target, vals.clone()).await;
        refused(&error, &expected);
        let model = broad.records("product.template").unwrap();
        let vals = vals
            .set_cell("name", text("Refused new product"))
            .set_cell("company_id", Cell::Id(a));
        let error = setup
            .savepoint(async |tx| model.create(tx, vec![vals]).await)
            .await
            .unwrap_err();
        refused(&error, &expected);
    }
    let shared = product(
        &mut setup,
        &broad,
        RawVals::new()
            .set_cell("name", text("Shared chosen taxes"))
            .set_commands(
                "taxes_id",
                [Command::set([
                    RecId::from_real(own_sale.ids()[0]).unwrap(),
                    RecId::from_real(foreign_sale.ids()[0]).unwrap(),
                ])],
            )
            .set_commands(
                "supplier_taxes_id",
                [Command::set([
                    RecId::from_real(own_purchase.ids()[0]).unwrap(),
                    RecId::from_real(foreign_purchase.ids()[0]).unwrap(),
                ])],
            ),
    )
    .await;
    let shared_id = shared.ids()[0];
    setup.install_audit_schema().await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let secured = secure(&db, &orm).await;
    let mut tx = secured
        .begin(erp_sdk::UserId::new(editor).unwrap())
        .await
        .unwrap();
    let principal = tx.env().principal().clone();
    let reader = tx.env().clone();
    let mut denied_messages = std::collections::BTreeMap::new();
    let target = reader
        .records("product.template")
        .unwrap()
        .browse([shared_id])
        .unwrap();
    for (field, foreign) in [
        ("taxes_id", foreign_sale.ids()[0]),
        ("supplier_taxes_id", foreign_purchase.ids()[0]),
    ] {
        let direct = reader
            .records("account.tax")
            .unwrap()
            .browse([foreign])
            .unwrap()
            .check_access(&mut tx, erp_sdk::Operation::Read)
            .await
            .unwrap_err();
        denied_messages.insert(
            field,
            format!(
                "Failed to write field product.template.{field}\n{}",
                direct.message()
            ),
        );
        let blank = reader.records("product.template").unwrap();
        let vals = RawVals::new()
            .set_cell("name", text("New foreign choice"))
            .set_commands(
                field.to_owned(),
                [Command::link(RecId::from_real(foreign).unwrap())],
            );
        let error = tx
            .savepoint(async |tx| blank.create(tx, vec![vals]).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            format!(
                "Failed to write field product.template.{field}\n{}",
                direct.message()
            )
        );
        // Remove readable choices while the hidden company's links stay in the database.
        target
            .write(
                &mut tx,
                RawVals::new().set_commands(field.to_owned(), [Command::set(Vec::<RecId>::new())]),
            )
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    drop(tx);
    let service = erp_rpc::RpcService::new(secured.clone(), RpcHost, erp_rpc::RpcConfig::default());
    for (field, foreign) in [
        ("taxes_id", foreign_sale.ids()[0]),
        ("supplier_taxes_id", foreign_purchase.ids()[0]),
    ] {
        // Use a separate unlinked product: a previously stored hidden link is preservation, not injection.
        let mut body = serde_json::json!({"vals_list":[{"name":"RPC foreign choice"}]});
        body["vals_list"][0][field] = serde_json::json!([[4, foreign, 0]]);
        let response = service
            .call(erp_rpc::RpcCall::json2(
                "/json/2/product.template/create",
                serde_json::to_vec(&body).unwrap(),
                erp_rpc::Caller::Principal(principal.clone()),
            ))
            .await;
        let error: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(response.status(), 403, "{error}");
        assert_eq!(error["name"], "erp.exceptions.AccessError");
        assert_eq!(error["message"], denied_messages[field]);
        let mut body = serde_json::json!({"ids":p.ids(),"vals":{}});
        body["vals"][field] = serde_json::json!([[4, foreign, 0]]);
        let response = service
            .call(erp_rpc::RpcCall::json2(
                "/json/2/product.template/write",
                serde_json::to_vec(&body).unwrap(),
                erp_rpc::Caller::Principal(principal.clone()),
            ))
            .await;
        let error: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(response.status(), 403, "{error}");
        assert_eq!(error["name"], "erp.exceptions.AccessError");
        assert_eq!(error["message"], denied_messages[field]);
    }
    let response=service.call(erp_rpc::RpcCall::json2("/json/2/product.template/write",serde_json::to_vec(&serde_json::json!({"ids":[shared_id],"vals":{"taxes_id":[[6,0,[]]],"supplier_taxes_id":[[6,0,[]]]}})).unwrap(),erp_rpc::Caller::Principal(principal.clone()))).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(response.body()).unwrap(),
        serde_json::Value::Bool(true)
    );
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    let stored = check
        .env()
        .records("product.template")
        .unwrap()
        .browse([shared_id])
        .unwrap();
    assert_eq!(
        stored.related(&mut check, "taxes_id").await.unwrap().ids(),
        foreign_sale.ids()
    );
    assert_eq!(
        stored
            .related(&mut check, "supplier_taxes_id")
            .await
            .unwrap()
            .ids(),
        foreign_purchase.ids()
    );
    check.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

const MOVE_REFUSAL: &str = "The product cannot move to this company: it still has taxes of other companies. Someone who can see those taxes must remove them first.";

#[test]
fn company_change_refuses_hidden_taxes_of_another_company() {
    big_stack(company_change);
}
async fn company_change() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, a) = fixture(&mut setup).await;
    let env = taxes.env().clone();
    let own_sale = tax(&taxes, &mut setup, "Own sale", "sale").await;
    let b = company(&mut setup, &env, "Other company", None).await;
    let country = own_sale.cell(&mut setup, "country_id").await.unwrap();
    let (foreign_sale, foreign_purchase) = other_taxes(&mut setup, &env, b, country).await;
    let linker = user(&mut setup, &env, "tax-link-b-user", &[b]).await;
    let mover = user(&mut setup, &env, "tax-link-a-manager", &[a]).await;
    let mut shared = Vec::new();
    for (name, sale) in [
        ("Hidden link product", Command::clear()),
        (
            "Clean shared product",
            Command::set([RecId::from_real(own_sale.ids()[0]).unwrap()]),
        ),
    ] {
        let created = product(
            &mut setup,
            &env,
            RawVals::new()
                .set_cell("name", text(name))
                .set_commands("taxes_id", [sale])
                .set_commands("supplier_taxes_id", [Command::clear()]),
        )
        .await;
        assert_eq!(
            created.cell(&mut setup, "company_id").await.unwrap(),
            Cell::Null
        );
        shared.push(created.ids()[0]);
    }
    setup.install_audit_schema().await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let secured = secure(&db, &orm).await;
    // A user of company B links B's taxes to the shared product.
    let mut tx = secured
        .begin(erp_sdk::UserId::new(linker).unwrap())
        .await
        .unwrap();
    tx.env()
        .records("product.template")
        .unwrap()
        .browse([shared[0]])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new()
                .set_commands(
                    "taxes_id",
                    [Command::link(
                        RecId::from_real(foreign_sale.ids()[0]).unwrap(),
                    )],
                )
                .set_commands(
                    "supplier_taxes_id",
                    [Command::link(
                        RecId::from_real(foreign_purchase.ids()[0]).unwrap(),
                    )],
                ),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // A product manager of company A only cannot see B's taxes and moves the product to A.
    let mut tx = secured
        .begin(erp_sdk::UserId::new(mover).unwrap())
        .await
        .unwrap();
    let templates = tx.env().records("product.template").unwrap();
    let hidden = templates.browse([shared[0]]).unwrap();
    assert!(
        hidden
            .related(&mut tx, "taxes_id")
            .await
            .unwrap()
            .is_empty()
    );
    let to_a = RawVals::new().set_cell("company_id", Cell::Id(a));
    refused(
        &write_error(&mut tx, &hidden, to_a.clone()).await,
        MOVE_REFUSAL,
    );
    let variant = hidden
        .related(&mut tx, "product_variant_ids")
        .await
        .unwrap();
    assert_eq!(variant.len(), 1);
    refused(
        &write_error(&mut tx, &variant, to_a.clone()).await,
        MOVE_REFUSAL,
    );
    // A new company product cannot carry a tax the writer cannot read.
    let vals = RawVals::new()
        .set_cell("name", text("Refused company product"))
        .set_cell("company_id", Cell::Id(a))
        .set_commands(
            "taxes_id",
            [Command::link(
                RecId::from_real(foreign_sale.ids()[0]).unwrap(),
            )],
        );
    let error = tx
        .savepoint(async |tx| templates.create(tx, vec![vals]).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{}", error.message());
    // Without a hidden link the same move succeeds.
    templates
        .browse([shared[1]])
        .unwrap()
        .write(&mut tx, to_a)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    let stored = check.env().records("product.template").unwrap();
    let hidden = stored.browse([shared[0]]).unwrap();
    assert_eq!(
        hidden.cell(&mut check, "company_id").await.unwrap(),
        Cell::Null
    );
    assert_eq!(
        hidden.related(&mut check, "taxes_id").await.unwrap().ids(),
        foreign_sale.ids()
    );
    assert_eq!(
        hidden
            .related(&mut check, "supplier_taxes_id")
            .await
            .unwrap()
            .ids(),
        foreign_purchase.ids()
    );
    let clean = stored.browse([shared[1]]).unwrap();
    assert_eq!(
        clean.cell(&mut check, "company_id").await.unwrap(),
        Cell::Id(a)
    );
    assert_eq!(
        clean.related(&mut check, "taxes_id").await.unwrap().ids(),
        own_sale.ids()
    );
    assert_eq!(
        id_of(
            &mut check,
            "SELECT count(*) FROM product_template WHERE name->>'en_US' = 'Refused company product'"
        )
        .await,
        0
    );
    check.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

const LINK_REFUSAL: &str = "The product cannot have these taxes: some belong to a company outside this product's company and its parent companies.";

#[test]
fn archived_foreign_tax_is_refused_on_company_products() {
    big_stack(archived_foreign);
}
async fn archived_foreign() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, a) = fixture(&mut tx).await;
    let env = taxes.env().clone();
    let own_sale = tax(&taxes, &mut tx, "Own sale", "sale").await;
    let b = company(&mut tx, &env, "Other company", None).await;
    let country = own_sale.cell(&mut tx, "country_id").await.unwrap();
    let (foreign_sale, foreign_purchase) = other_taxes(&mut tx, &env, b, country).await;
    for archived in [&foreign_sale, &foreign_purchase] {
        archived
            .write(
                &mut tx,
                RawVals::new().set_cell("active", Cell::Bool(false)),
            )
            .await
            .unwrap();
    }
    // Both companies selected: the archived taxes are readable, only the link read hides them.
    let broad = env.with_context([(
        "allowed_company_ids",
        Value::list([Value::Int(a), Value::Int(b)]),
    )]);
    let p = product(
        &mut tx,
        &broad,
        RawVals::new()
            .set_cell("name", text("Company A product"))
            .set_cell("company_id", Cell::Id(a)),
    )
    .await;
    let variant = p.related(&mut tx, "product_variant_ids").await.unwrap();
    assert_eq!(variant.len(), 1);
    for (field, archived) in [
        ("taxes_id", &foreign_sale),
        ("supplier_taxes_id", &foreign_purchase),
    ] {
        let link = RawVals::new().set_commands(
            field.to_owned(),
            [Command::link(RecId::from_real(archived.ids()[0]).unwrap())],
        );
        refused(&write_error(&mut tx, &p, link.clone()).await, LINK_REFUSAL);
        refused(
            &write_error(&mut tx, &variant, link.clone()).await,
            LINK_REFUSAL,
        );
        for model in ["product.template", "product.product"] {
            let model = broad.records(model).unwrap();
            let vals = link
                .clone()
                .set_cell("name", text("Refused archived link"))
                .set_cell("company_id", Cell::Id(a));
            let error = tx
                .savepoint(async |tx| model.create(tx, vec![vals]).await)
                .await
                .unwrap_err();
            refused(&error, LINK_REFUSAL);
        }
    }
    assert_eq!(
        id_of(
            &mut tx,
            &format!(
                "SELECT (SELECT count(*) FROM product_taxes_rel WHERE prod_id = {0}) \
                 + (SELECT count(*) FROM product_supplier_taxes_rel WHERE prod_id = {0})",
                p.ids()[0]
            )
        )
        .await,
        0
    );
    assert_eq!(
        id_of(
            &mut tx,
            "SELECT count(*) FROM product_template WHERE name->>'en_US' = 'Refused archived link'"
        )
        .await,
        0
    );
    // The own company's tax still links.
    p.write(
        &mut tx,
        RawVals::new().set_commands(
            "taxes_id",
            [Command::link(RecId::from_real(own_sale.ids()[0]).unwrap())],
        ),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn shared_product_links_follow_selected_companies_and_switched_users() {
    big_stack(shared_selection);
}
async fn shared_selection() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, a) = fixture(&mut setup).await;
    let env = taxes.env().clone();
    let own_sale = tax(&taxes, &mut setup, "Own sale", "sale").await;
    let own_purchase = tax(&taxes, &mut setup, "Own purchase", "purchase").await;
    defaults_on(
        &mut setup,
        &env,
        a,
        own_sale.ids()[0],
        own_purchase.ids()[0],
    )
    .await;
    let b = company(&mut setup, &env, "Other company", None).await;
    let country = own_sale.cell(&mut setup, "country_id").await.unwrap();
    let (foreign_sale, foreign_purchase) = other_taxes(&mut setup, &env, b, country).await;
    let both = user(&mut setup, &env, "tax-link-two-companies", &[a, b]).await;
    let editor = user(&mut setup, &env, "tax-link-switched", &[a]).await;
    let shared = product(
        &mut setup,
        &env,
        RawVals::new()
            .set_cell("name", text("Shared selection"))
            .set_commands("taxes_id", [Command::clear()])
            .set_commands("supplier_taxes_id", [Command::clear()]),
    )
    .await;
    assert_eq!(
        shared.cell(&mut setup, "company_id").await.unwrap(),
        Cell::Null
    );
    let shared = shared.ids()[0];
    // The admission refuses by name a readable tax of a company that is neither selected nor
    // an ancestor of one. Only a rule-free environment can read such a tax: see the RPC part.
    let a_only = env.with_context([("allowed_company_ids", Value::list([Value::Int(a)]))]);
    let target = a_only
        .records("product.template")
        .unwrap()
        .browse([shared])
        .unwrap();
    for (field, label, tax) in [
        ("taxes_id", SALE_LABEL, &foreign_sale),
        ("supplier_taxes_id", PURCHASE_LABEL, &foreign_purchase),
    ] {
        let expected = link_refusal(&mut setup, tax, label).await;
        let link = RawVals::new().set_commands(
            field.to_owned(),
            [Command::link(RecId::from_real(tax.ids()[0]).unwrap())],
        );
        refused(
            &write_error(&mut setup, &target, link.clone()).await,
            &expected,
        );
        let model = a_only.records("product.template").unwrap();
        let vals = link.set_cell("name", text("Refused shared product"));
        let error = setup
            .savepoint(async |tx| model.create(tx, vec![vals]).await)
            .await
            .unwrap_err();
        refused(&error, &expected);
    }
    setup.install_audit_schema().await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let secured = secure(&db, &orm).await;
    let principal = secured
        .begin(erp_sdk::UserId::new(both).unwrap())
        .await
        .unwrap()
        .env()
        .principal()
        .clone();
    let service = erp_rpc::RpcService::new(secured.clone(), RpcHost, erp_rpc::RpcConfig::default());
    // Over RPC no caller reaches that refusal on a shared product. `account.tax` declares the
    // generated company restriction `company_access = direct(field = "company_id", branch =
    // "ancestor", empty = "deny")` (addons/account/src/tax/mod.rs), which reads as
    // `('company_id', 'parent_of', company_ids)` with `company_ids` bound to the validated selected
    // companies (erp-security company_access.rs, erp-orm access/engine.rs). A shared product's
    // admission chain is exactly those companies and their ancestors, so a tax outside it is
    // unreadable to every non-elevated caller and is refused by the read check first, as here
    // for a member of both companies who selected only A. Selecting B as well admits the link.
    for (field, foreign) in [
        ("taxes_id", foreign_sale.ids()[0]),
        ("supplier_taxes_id", foreign_purchase.ids()[0]),
    ] {
        for (selected, status) in [(vec![a], 403), (vec![a, b], 200)] {
            let mut body = serde_json::json!({
                "ids": [shared],
                "vals": {},
                "context": {"allowed_company_ids": selected},
            });
            body["vals"][field] = serde_json::json!([[4, foreign, 0]]);
            let response = service
                .call(erp_rpc::RpcCall::json2(
                    "/json/2/product.template/write",
                    serde_json::to_vec(&body).unwrap(),
                    erp_rpc::Caller::Principal(principal.clone()),
                ))
                .await;
            let answer: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
            assert_eq!(response.status(), status, "{answer}");
            if status == 403 {
                assert_eq!(answer["name"], "erp.exceptions.AccessError");
                assert!(
                    answer["message"]
                        .as_str()
                        .unwrap()
                        .starts_with(&format!("Failed to write field product.template.{field}\n")),
                    "{answer}"
                );
            } else {
                assert_eq!(answer, serde_json::Value::Bool(true));
            }
        }
    }
    // An environment made by with_user has no companies of its own; the admission and the
    // defaults use the transaction's validated companies of the switched user.
    let mut tx = secured.begin(SUPERUSER_ID).await.unwrap();
    let switched = tx.env().with_user(editor);
    assert!(switched.companies().unwrap().is_empty());
    let model = switched.records("product.template").unwrap();
    let defaults = model
        .default_get(&mut tx, &["taxes_id", "supplier_taxes_id"])
        .await
        .unwrap();
    let from_defaults = model
        .create(
            &mut tx,
            vec![
                model
                    .vals_from_values(&defaults)
                    .unwrap()
                    .set_cell("name", text("Switched defaults")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        from_defaults
            .related(&mut tx, "taxes_id")
            .await
            .unwrap()
            .ids(),
        own_sale.ids()
    );
    assert_eq!(
        from_defaults
            .related(&mut tx, "supplier_taxes_id")
            .await
            .unwrap()
            .ids(),
        own_purchase.ids()
    );
    let explicit = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Switched explicit"))
                    .set_commands(
                        "taxes_id",
                        [Command::set([RecId::from_real(own_sale.ids()[0]).unwrap()])],
                    )
                    .set_commands("supplier_taxes_id", [Command::clear()]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        explicit.related(&mut tx, "taxes_id").await.unwrap().ids(),
        own_sale.ids()
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn branch_defaults_use_stored_effective_fields_and_omit_unreadable_tax() {
    big_stack(branch_defaults);
}
async fn branch_defaults() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, root) = fixture(&mut setup).await;
    let env = taxes.env().clone();
    let sale = tax(&taxes, &mut setup, "Branch root sale", "sale").await;
    let purchase = tax(&taxes, &mut setup, "Branch root purchase", "purchase").await;
    defaults_on(&mut setup, &env, root, sale.ids()[0], purchase.ids()[0]).await;
    let alternate = tax(&taxes, &mut setup, "Alternate inherited sale", "sale").await;
    let inherited_product = product(
        &mut setup,
        &env,
        RawVals::new()
            .set_cell("name", text("Ancestor company product"))
            .set_cell("company_id", Cell::Id(root))
            .set_commands("supplier_taxes_id", [Command::clear()]),
    )
    .await;
    let branch = company(&mut setup, &env, "Restricted branch", Some(root)).await;
    let editor = user(&mut setup, &env, "branch-product-editor", &[branch]).await;
    let model_id: i64 = setup
        .query("SELECT id FROM ir_model WHERE model='account.tax'", &[])
        .await
        .unwrap()[0]
        .get(0);
    env.records("ir.access")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", text("Hide a root default"))
                    .set_cell("model_id", Cell::Id(model_id))
                    .set_cell("operation", text("r"))
                    .set_cell(
                        "domain",
                        text(&format!("[('id', '!=', {})]", purchase.ids()[0])),
                    ),
            ],
        )
        .await
        .unwrap();
    setup.install_audit_schema().await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let secured = secure(&db, &orm).await;
    secured.check_audit_store().await.unwrap();
    let mut tx = secured
        .begin(erp_sdk::UserId::new(editor).unwrap())
        .await
        .unwrap();
    let own = tx.env().clone();
    assert!(
        !own.records("res.company")
            .unwrap()
            .browse([root])
            .unwrap()
            .has_access(&mut tx, erp_sdk::Operation::Read)
            .await
            .unwrap()
    );
    let p = product(
        &mut tx,
        &own,
        RawVals::new().set_cell("name", text("Effective branch defaults")),
    )
    .await;
    assert_eq!(
        p.related(&mut tx, "taxes_id").await.unwrap().ids(),
        sale.ids()
    );
    assert!(
        p.related(&mut tx, "supplier_taxes_id")
            .await
            .unwrap()
            .is_empty()
    );
    let inherited = inherited_product.with_env(own.clone());
    inherited
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "taxes_id",
                [Command::set(
                    [RecId::from_real(alternate.ids()[0]).unwrap()],
                )],
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        inherited.related(&mut tx, "taxes_id").await.unwrap().ids(),
        alternate.ids()
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    let count:i64=check.query("SELECT count(*)::int8 FROM erp_event.ir_audit_entry e CROSS JOIN LATERAL jsonb_array_elements(e.events) event WHERE event->>'type'='elevation' AND event->>'reason'='account.product_tax_root_defaults'",&[]).await.unwrap()[0].get(0);
    assert_eq!(count, 0, "effective fields require no elevation");
    check.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn tax_link_views_are_served_from_installed_extensions() {
    big_stack(served_link_views);
}
async fn served_link_views() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let root = main_company(&mut setup).await;
    let setup_env = setup.env().with_company(root).unwrap();
    let editor = user(&mut setup, &setup_env, "tax-view-editor", &[root]).await;
    let groups: Vec<RecId> = setup.query("SELECT res_id FROM ir_model_data WHERE (module='base' AND name='group_system') OR (module='account' AND name='group_account_manager')", &[]).await.unwrap().iter().map(|row| RecId::from_real(row.get(0)).unwrap()).collect();
    setup_env
        .records("res.users")
        .unwrap()
        .browse([editor])
        .unwrap()
        .write(
            &mut setup,
            RawVals::new().set_commands("group_ids", groups.into_iter().map(Command::link)),
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let secured = secure(&db, &orm).await;
    let mut tx = secured
        .begin(erp_sdk::UserId::new(editor).unwrap())
        .await
        .unwrap();
    let env = tx.env().clone();
    let service = erp_rpc::RpcService::new(secured.clone(), RpcHost, erp_rpc::RpcConfig::default());
    let mut descriptions = serde_json::Map::new();
    for (model, expected) in [
        ("product.template", vec!["taxes_id", "supplier_taxes_id"]),
        (
            "product.template.attribute.line",
            vec!["attribute_id", "value_ids"],
        ),
        ("account.code.mapping", vec!["company_id", "code"]),
        ("account.account", vec!["tax_ids"]),
        (
            "res.company",
            vec!["account_sale_tax_id", "account_purchase_tax_id"],
        ),
    ] {
        let requests = ["list", "form"]
            .into_iter()
            .map(|kind| Value::list([Value::False, Value::str(kind)]))
            .collect();
        let views = erp_addon_web::metadata_test_support::test_get_views(
            &env,
            &mut tx,
            model,
            requests,
            32 * 1024 * 1024,
        )
        .await
        .unwrap_or_else(|error| panic!("{model}: {error:?}"));
        let json = serde_json::to_value(views).unwrap();
        let view_text = json["views"].to_string();
        for field in expected {
            assert!(
                view_text.contains(&format!("\"name\":\"{field}\"")),
                "missing served field {model}.{field}: {view_text}"
            );
        }
        for mobile in [false, true] {
            let response = service.call(erp_rpc::RpcCall::json2(
                &format!("/json/2/{model}/get_views"),
                serde_json::to_vec(&serde_json::json!({"views":[[null,"list"],[null,"form"]],"wire_major":1,"options":{"mobile":mobile}})).unwrap(),
                erp_rpc::Caller::Principal(env.principal().clone()),
            )).await;
            assert_eq!(
                response.status(),
                200,
                "{model} mobile={mobile}: {}",
                String::from_utf8_lossy(response.body())
            );
        }
        descriptions.insert(model.to_owned(), json);
    }
    std::fs::write(
        "/tmp/a later release",
        serde_json::to_vec(&descriptions).unwrap(),
    )
    .unwrap();
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn product_tax_defaults_preserve_persisted_and_contextual_empty_choices() {
    big_stack(persisted_defaults);
}
async fn persisted_defaults() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, root) = fixture(&mut tx).await;
    let env = taxes.env().clone();
    let sale = tax(&taxes, &mut tx, "Persisted default sale", "sale").await;
    let purchase = tax(&taxes, &mut tx, "Persisted default purchase", "purchase").await;
    defaults_on(&mut tx, &env, root, sale.ids()[0], purchase.ids()[0]).await;
    let context = env.with_context([("default_taxes_id", Value::list(Vec::<Value>::new()))]);
    let p = product(
        &mut tx,
        &context,
        RawVals::new().set_cell("name", text("Context empty choice")),
    )
    .await;
    assert!(p.related(&mut tx, "taxes_id").await.unwrap().is_empty());
    assert_eq!(
        p.related(&mut tx, "supplier_taxes_id").await.unwrap().ids(),
        purchase.ids()
    );
    let field_id: i64 = tx
        .query(
            "SELECT id FROM ir_model_fields WHERE model='product.template' AND name='taxes_id'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    env.records("ir.default")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("field_id", Cell::Id(field_id))
                    .set_cell("json_value", text("[]")),
            ],
        )
        .await
        .unwrap();
    let p = product(
        &mut tx,
        &env,
        RawVals::new().set_cell("name", text("Persisted empty choice")),
    )
    .await;
    assert!(p.related(&mut tx, "taxes_id").await.unwrap().is_empty());
    assert_eq!(
        p.related(&mut tx, "supplier_taxes_id").await.unwrap().ids(),
        purchase.ids()
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn company_set_product_defaults_keep_own_company_with_multiple_selections() {
    big_stack(own_company_defaults);
}
async fn own_company_defaults() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, a) = fixture(&mut tx).await;
    let env = taxes.env().clone();
    let sale = tax(&taxes, &mut tx, "Own company sale", "sale").await;
    let purchase = tax(&taxes, &mut tx, "Own company purchase", "purchase").await;
    defaults_on(&mut tx, &env, a, sale.ids()[0], purchase.ids()[0]).await;
    let b = company(&mut tx, &env, "Second defaults company", None).await;
    let country = sale.cell(&mut tx, "country_id").await.unwrap();
    let (bs, bp) = other_taxes(&mut tx, &env, b, country).await;
    defaults_on(&mut tx, &env, b, bs.ids()[0], bp.ids()[0]).await;
    let both = env.with_context([(
        "allowed_company_ids",
        Value::list([Value::Int(a), Value::Int(b)]),
    )]);
    let p = product(
        &mut tx,
        &both,
        RawVals::new()
            .set_cell("name", text("Company owned defaults"))
            .set_cell("company_id", Cell::Id(a)),
    )
    .await;
    assert_eq!(
        p.related(&mut tx, "taxes_id").await.unwrap().ids(),
        sale.ids()
    );
    assert_eq!(
        p.related(&mut tx, "supplier_taxes_id").await.unwrap().ids(),
        purchase.ids()
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn stored_company_tax_defaults_follow_parent_changes_and_keep_own_override() {
    big_stack(effective_changes);
}
async fn effective_changes() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, root) = fixture(&mut tx).await;
    let env = taxes.env().clone();
    let first = tax(&taxes, &mut tx, "First effective sale", "sale").await;
    let second = tax(&taxes, &mut tx, "Second effective sale", "sale").await;
    let purchase = tax(&taxes, &mut tx, "Effective purchase", "purchase").await;
    defaults_on(&mut tx, &env, root, first.ids()[0], purchase.ids()[0]).await;
    let branch = company(&mut tx, &env, "Effective child", Some(root)).await;
    let leaf = company(&mut tx, &env, "Effective grandchild", Some(branch)).await;
    let c = env
        .records("res.company")
        .unwrap()
        .browse([branch])
        .unwrap();
    let l = env.records("res.company").unwrap().browse([leaf]).unwrap();
    assert_eq!(
        c.cell(&mut tx, "effective_account_sale_tax_id")
            .await
            .unwrap(),
        Cell::Id(first.ids()[0])
    );
    assert_eq!(
        l.cell(&mut tx, "effective_account_sale_tax_id")
            .await
            .unwrap(),
        Cell::Id(first.ids()[0])
    );
    defaults_on(&mut tx, &env, root, second.ids()[0], purchase.ids()[0]).await;
    assert_eq!(
        l.cell(&mut tx, "effective_account_sale_tax_id")
            .await
            .unwrap(),
        Cell::Id(second.ids()[0])
    );
    c.write(
        &mut tx,
        RawVals::new().set_cell("account_sale_tax_id", Cell::Id(first.ids()[0])),
    )
    .await
    .unwrap();
    defaults_on(&mut tx, &env, root, second.ids()[0], purchase.ids()[0]).await;
    assert_eq!(
        c.cell(&mut tx, "effective_account_sale_tax_id")
            .await
            .unwrap(),
        Cell::Id(first.ids()[0])
    );
    assert_eq!(
        l.cell(&mut tx, "effective_account_sale_tax_id")
            .await
            .unwrap(),
        Cell::Id(first.ids()[0])
    );
    tx.flush().await.unwrap();
    let stored: i64 = tx
        .query(
            "SELECT effective_account_sale_tax_id FROM res_company WHERE id=$1",
            &[&leaf],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, first.ids()[0]);
    let e = write_error(
        &mut tx,
        &c,
        RawVals::new().set_cell("effective_account_sale_tax_id", Cell::Null),
    )
    .await;
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "Effective company tax defaults are computed and cannot be assigned."
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn effective_defaults_recompute_outside_administrators_company_membership() {
    big_stack(effective_admin);
}
async fn effective_admin() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, root) = fixture(&mut setup).await;
    let env = taxes.env().clone();
    let first = tax(&taxes, &mut setup, "Admin first sale", "sale").await;
    let second = tax(&taxes, &mut setup, "Admin second sale", "sale").await;
    let purchase = tax(&taxes, &mut setup, "Admin purchase", "purchase").await;
    defaults_on(&mut setup, &env, root, first.ids()[0], purchase.ids()[0]).await;
    let branch = company(&mut setup, &env, "Outside admin membership", Some(root)).await;
    user(&mut setup, &env, "root-catalog-editor", &[root]).await;
    let admin: i64 = setup
        .query(
            "SELECT id FROM res_users WHERE login='tax-link-administrator'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    setup.commit().await.unwrap();
    drop(setup);
    let secured = secure(&db, &orm).await;
    let mut tx = secured
        .begin(erp_sdk::UserId::new(admin).unwrap())
        .await
        .unwrap();
    let admin_env = tx.env().clone();
    assert_eq!(
        admin_env
            .companies()
            .unwrap()
            .iter()
            .map(|c| c.get())
            .collect::<Vec<_>>(),
        vec![root]
    );
    let own = admin_env
        .records("res.users")
        .unwrap()
        .browse([admin])
        .unwrap();
    assert_eq!(
        own.related(&mut tx, "company_ids").await.unwrap().ids(),
        vec![root]
    );
    defaults_on(
        &mut tx,
        &admin_env,
        root,
        second.ids()[0],
        purchase.ids()[0],
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);
    let mut verify = orm.begin(SUPERUSER_ID).await.unwrap();
    let stored: i64 = verify
        .query(
            "SELECT effective_account_sale_tax_id FROM res_company WHERE id=$1",
            &[&branch],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, second.ids()[0]);
    let c = verify
        .env()
        .records("res.company")
        .unwrap()
        .browse([branch])
        .unwrap();
    let e = write_error(
        &mut verify,
        &c,
        RawVals::new().set_cell("parent_id", Cell::Null),
    )
    .await;
    assert_eq!(e.kind(), ErrorKind::UserError);
    assert_eq!(e.message(), "The company hierarchy cannot be changed.");
    assert_eq!(
        c.cell(&mut verify, "effective_account_sale_tax_id")
            .await
            .unwrap(),
        Cell::Id(second.ids()[0])
    );
    verify.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn product_installs_independently_without_account_tax_fields() {
    big_stack(product_without_account);
}
async fn product_without_account() {
    let (db, orm) = install_modules(&["product"]).await;
    assert!(orm.registry().model("account.tax").is_none());
    let model = orm.registry().model("product.template").unwrap();
    assert!(model.field("taxes_id").is_none());
    assert!(model.field("supplier_taxes_id").is_none());
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let c = main_company(&mut tx).await;
    let env = tx.env().with_company(c).unwrap();
    let p = product(
        &mut tx,
        &env,
        RawVals::new().set_cell("name", text("Independent catalog")),
    )
    .await;
    assert_eq!(
        p.related(&mut tx, "product_variant_ids")
            .await
            .unwrap()
            .len(),
        1
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn account_delete_refusal_counts_unreadable_accounts_without_labels() {
    big_stack(delete_count);
}
async fn delete_count() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, root) = fixture(&mut setup).await;
    let env = taxes.env().clone();
    let t = tax(&taxes, &mut setup, "Protected distribution tax", "sale").await;
    let a = account(
        &env,
        &mut setup,
        "Unreadable distribution account",
        "liability_current",
    )
    .await;
    t.related(&mut setup, "repartition_line_ids")
        .await
        .unwrap()
        .write(
            &mut setup,
            RawVals::new().set_cell("account_id", Cell::Id(a.ids()[0])),
        )
        .await
        .unwrap();
    let editor = user(&mut setup, &env, "unlink-account-manager", &[root]).await;
    let group:i64=setup.query("SELECT res_id FROM ir_model_data WHERE module='account' AND name='group_account_manager'",&[]).await.unwrap()[0].get(0);
    env.records("res.users")
        .unwrap()
        .browse([editor])
        .unwrap()
        .write(
            &mut setup,
            RawVals::new().set_commands(
                "group_ids",
                [Command::link(RecId::from_real(group).unwrap())],
            ),
        )
        .await
        .unwrap();
    let model: i64 = setup
        .query("SELECT id FROM ir_model WHERE model='account.account'", &[])
        .await
        .unwrap()[0]
        .get(0);
    env.records("ir.access")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", text("Hide the account label"))
                    .set_cell("model_id", Cell::Id(model))
                    .set_cell("operation", text("r"))
                    .set_cell("domain", text(&format!("[('id','!=',{})]", a.ids()[0]))),
            ],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let secured = secure(&db, &orm).await;
    let mut tx = secured
        .begin(erp_sdk::UserId::new(editor).unwrap())
        .await
        .unwrap();
    let target = tx
        .env()
        .records("account.account")
        .unwrap()
        .browse(a.ids())
        .unwrap();
    assert!(
        !target
            .has_access(&mut tx, erp_sdk::Operation::Read)
            .await
            .unwrap()
    );
    target
        .check_access(&mut tx, erp_sdk::Operation::Unlink)
        .await
        .unwrap();
    let e = target.unlink(&mut tx).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::UserError);
    assert_eq!(
        e.message(),
        "1 account(s) cannot be deleted because tax distributions still refer to them."
    );
    tx.rollback().await.unwrap();
    db.cleanup().await.unwrap();
}

#[test]
fn repartition_fk_closes_cross_transaction_precheck_delete_race() {
    big_stack(delete_race);
}
async fn delete_race() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, _) = fixture(&mut setup).await;
    let tax = tax(&taxes, &mut setup, "Concurrent distribution", "sale").await;
    let account = account(
        taxes.env(),
        &mut setup,
        "Concurrent account",
        "liability_current",
    )
    .await;
    let account_id = account.ids()[0];
    let line_id = tax
        .related(&mut setup, "repartition_line_ids")
        .await
        .unwrap()
        .ids()[0];
    setup.commit().await.unwrap();
    drop(setup);
    // Hold one transaction after exactly the precheck used by unlink. A different transaction
    // establishes the reference before the first reaches DELETE; the FK must close this race.
    let mut deleting = db.pool().get().await.unwrap();
    let transaction = deleting.transaction().await.unwrap();
    assert!(
        transaction
            .query(
                "SELECT 1 FROM account_tax_repartition_line WHERE account_id = $1",
                &[&account_id]
            )
            .await
            .unwrap()
            .is_empty()
    );
    let mut inserting = orm.begin(SUPERUSER_ID).await.unwrap();
    inserting
        .env()
        .records("account.tax.repartition.line")
        .unwrap()
        .browse([line_id])
        .unwrap()
        .write(
            &mut inserting,
            RawVals::new().set_cell("account_id", Cell::Id(account_id)),
        )
        .await
        .unwrap();
    inserting.commit().await.unwrap();
    drop(inserting);
    let constraint: String = transaction.query_one("SELECT conname FROM pg_constraint WHERE conrelid='account_tax_repartition_line'::regclass AND confrelid='account_account'::regclass AND contype='f'", &[]).await.unwrap().get(0);
    let error = transaction
        .execute("DELETE FROM account_account WHERE id=$1", &[&account_id])
        .await
        .unwrap_err();
    let server = error.as_db_error().unwrap();
    assert_eq!(
        server.code(),
        &tokio_postgres::error::SqlState::RESTRICT_VIOLATION
    );
    assert_eq!(
        server.message(),
        format!(
            "update or delete on table \"account_account\" violates RESTRICT setting of foreign key constraint \"{constraint}\" on table \"account_tax_repartition_line\""
        )
    );
    transaction.rollback().await.unwrap();
    assert!(
        deleting
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM account_account WHERE id=$1)",
                &[&account_id]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(deleting);
    db.cleanup().await.unwrap();
}
