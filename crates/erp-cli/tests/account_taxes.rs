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
    install_with_used(false).await
}
async fn install_with_used(used: bool) -> (TestDb, Orm) {
    let db = TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons_dir());
    index
        .add_dev_path(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tax_used_addons"));
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
            &used_probe::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let outcome = Box::pin(system.button_immediate_install(
        &env,
        &[
            ModuleName::new(if used { "test_tax_used" } else { "account" }).unwrap(),
            ModuleName::new("web").unwrap(),
        ],
    ))
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

#[test]
fn tax_installation_and_defaults() {
    big_stack(defaults);
}
async fn defaults() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let company = main_company(&mut tx).await;
    let env = tx.env().with_company(company).unwrap();
    let taxes = env
        .records("account.tax")
        .expect("account installation must register taxes");
    assert_eq!(taxes.search(&mut tx, &Domain::TRUE).await.unwrap().len(), 0);
    for model in ["account.tax.group", "account.tax.repartition.line"] {
        assert_eq!(
            env.records(model)
                .unwrap()
                .search_count(&mut tx, &Domain::TRUE, None)
                .await
                .unwrap(),
            0,
            "no standalone jurisdiction seed for {model}"
        );
    }

    let country = tx
        .query("SELECT id FROM res_country WHERE code='US'", &[])
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    env.records("res.company")
        .unwrap()
        .browse([company])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("account_fiscal_country_id", Cell::Id(country)),
        )
        .await
        .unwrap();
    let group = env
        .records("account.tax.group")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Sales taxes".into()))],
        )
        .await
        .unwrap();
    let proposed = taxes
        .default_get(
            &mut tx,
            &[
                "company_id",
                "country_id",
                "tax_group_id",
                "invoice_repartition_line_ids",
                "refund_repartition_line_ids",
            ],
        )
        .await
        .unwrap();
    assert_eq!(proposed.get("company_id"), Some(&Value::Int(company)));
    assert_eq!(proposed.get("country_id"), Some(&Value::Int(country)));
    assert_eq!(
        proposed.get("tax_group_id"),
        Some(&Value::Int(group.ids()[0]))
    );
    for field in [
        "invoice_repartition_line_ids",
        "refund_repartition_line_ids",
    ] {
        let Value::List(commands) = proposed.get(field).expect("draft distribution defaults")
        else {
            panic!("commands");
        };
        assert_eq!(commands.len(), 2);
    }
    let tax = taxes
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("VAT".into()))],
        )
        .await
        .unwrap();
    assert_eq!(
        tax.cell(&mut tx, "type_tax_use").await.unwrap(),
        Cell::Str("sale".into())
    );
    assert_eq!(
        tax.cell(&mut tx, "country_id").await.unwrap(),
        Cell::Id(country)
    );
    assert_eq!(
        tax.cell(&mut tx, "tax_group_id").await.unwrap(),
        Cell::Id(group.ids()[0])
    );
    assert_eq!(
        tax.cell(&mut tx, "root_company_id").await.unwrap(),
        Cell::Id(company)
    );
    let indexes = tx.query("SELECT pg_get_indexdef(i.indexrelid), pg_get_expr(i.indpred,i.indrelid) FROM pg_index i JOIN pg_class t ON t.oid=i.indrelid WHERE t.relname='account_tax' AND i.indisunique AND i.indpred IS NOT NULL", &[]).await.unwrap();
    assert_eq!(indexes.len(), 1);
    let definition: String = indexes[0].get(0);
    let predicate: String = indexes[0].get(1);
    for expected in [
        "root_company_id",
        "name ->> 'en_US'::text",
        "type_tax_use",
        "COALESCE(tax_scope",
        "country_id",
    ] {
        assert!(definition.contains(expected), "{definition}");
    }
    assert!(
        predicate.contains("active")
            && predicate.contains("type_tax_use")
            && predicate.contains("none"),
        "{predicate}"
    );
    // the key's preconditions (K2): a NULL root, country or active flag would escape it
    for column in ["root_company_id", "country_id", "active"] {
        let nullable: String = tx
            .query(
                "SELECT is_nullable::text FROM information_schema.columns WHERE table_name = 'account_tax' AND column_name = $1",
                &[&column],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(nullable, "NO", "account_tax.{column}");
    }
    for field in ["root_company_id", "repartition_serial"] {
        refused_write(
            &mut tx,
            &tax,
            RawVals::new().set_cell(
                field,
                if field == "root_company_id" {
                    Cell::Id(company)
                } else {
                    Cell::Int(9)
                },
            ),
            "root_company_id and repartition_serial are derived tax fields and cannot be supplied.",
        )
        .await;
    }

    assert_eq!(
        tax.related(&mut tx, "repartition_line_ids")
            .await
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        tax.cell(&mut tx, "include_base_amount").await.unwrap(),
        Cell::Bool(false)
    );
    assert_eq!(
        tax.cell(&mut tx, "is_base_affected").await.unwrap(),
        Cell::Bool(true)
    );
    let erp_sdk::FieldType::Selection {
        values: Some(kinds),
        ..
    } = &taxes.spec().field("amount_type").unwrap().ty
    else {
        panic!("amount type selection")
    };
    assert_eq!(
        kinds
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["percent", "fixed", "division", "group"]
    );
    let erp_sdk::FieldType::Selection {
        values: Some(uses), ..
    } = &taxes.spec().field("type_tax_use").unwrap().ty
    else {
        panic!("use selection")
    };
    assert_eq!(
        uses.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
        ["sale", "purchase", "none"]
    );
    let lines = tax.related(&mut tx, "repartition_line_ids").await.unwrap();
    for line in lines.iter() {
        assert_eq!(
            line.cell(&mut tx, "factor_percent").await.unwrap(),
            decimal("100")
        );
    }
    for doc in ["invoice", "refund"] {
        let side = lines
            .filtered_domain(
                &mut tx,
                &Domain::condition("document_type", Operator::Eq, doc).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(side.len(), 2);
        assert_eq!(
            side.cells(&mut tx, "repartition_type").await.unwrap(),
            [text("base"), text("tax")]
        );
    }
    let other_country: i64 = tx
        .query("SELECT id FROM res_country WHERE code='DE'", &[])
        .await
        .unwrap()[0]
        .get(0);
    let neutral = env
        .records("account.tax.group")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Neutral group"))
                    .set_cell("country_id", Cell::Null),
            ],
        )
        .await
        .unwrap();
    let context_tax = env
        .with_context([("default_country_id", Value::Int(other_country))])
        .records("account.tax")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", text("Context country tax"))],
        )
        .await
        .unwrap();
    assert_eq!(
        context_tax.cell(&mut tx, "country_id").await.unwrap(),
        Cell::Id(other_country)
    );
    assert_eq!(
        context_tax.cell(&mut tx, "tax_group_id").await.unwrap(),
        Cell::Id(neutral.ids()[0])
    );
    let company_record = env
        .records("res.company")
        .unwrap()
        .browse([company])
        .unwrap();
    company_record
        .write(
            &mut tx,
            RawVals::new().set_cell("account_fiscal_country_id", Cell::Null),
        )
        .await
        .unwrap();
    company_record
        .related(&mut tx, "partner_id")
        .await
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("country_id", Cell::Id(country)),
        )
        .await
        .unwrap();
    let fallback = make(&taxes, &mut tx, "Country fallback").await;
    assert_eq!(
        fallback.cell(&mut tx, "country_id").await.unwrap(),
        Cell::Id(country)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

fn text(s: &str) -> Cell {
    Cell::Str(s.into())
}
fn decimal(s: &str) -> Cell {
    Cell::Decimal(s.parse().unwrap())
}
async fn fixture(tx: &mut Transaction) -> (Records, i64) {
    let company = main_company(tx).await;
    let env = tx.env().with_company(company).unwrap();
    let country = tx
        .query("SELECT id FROM res_country WHERE code='US'", &[])
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
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
        .create(tx, vec![RawVals::new().set_cell("name", text("Tax group"))])
        .await
        .unwrap();
    (env.records("account.tax").unwrap(), company)
}
async fn make(taxes: &Records, tx: &mut Transaction, name: &str) -> Records {
    taxes
        .create(tx, vec![RawVals::new().set_cell("name", text(name))])
        .await
        .unwrap()
}
/// The refusal of a duplicate tax name (K2), our own wording.
const DUPLICATE: &str = "Each active tax name can be used once per company tree, tax type, tax scope and country: rename the tax or archive the existing one.";
/// The pre-check's refusal for one duplicate: the sentence, then the name and its company.
fn duplicate(name: &str, company: &str) -> String {
    format!("{DUPLICATE}\n\"{name}\" is already used in {company}.")
}
async fn refused_write(tx: &mut Transaction, record: &Records, vals: RawVals, message: &str) {
    let record = record.clone();
    let e = tx
        .savepoint(async move |tx| record.write(tx, vals).await)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation, "{e:?}");
    assert_eq!(e.message(), message);
}
#[test]
fn tax_unique_names_and_decimal_scalars() {
    big_stack(unique);
}
async fn unique() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, company) = fixture(&mut tx).await;
    let tax = make(&taxes, &mut tx, "VAT").await;
    let company_name = taxes
        .env()
        .records("res.company")
        .unwrap()
        .browse([company])
        .unwrap()
        .cell(&mut tx, "name")
        .await
        .unwrap();
    let Cell::Str(company_name) = company_name else {
        panic!("company name")
    };
    let model = taxes.clone();
    let e = tx
        .savepoint(async move |tx| {
            model
                .create(tx, vec![RawVals::new().set_cell("name", text("VAT"))])
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.message(), duplicate("VAT", &company_name));
    tax.write(
        &mut tx,
        RawVals::new()
            .set_cell("amount", decimal("12.3456"))
            .set_cell("invoice_label", text("GST")),
    )
    .await
    .unwrap();
    assert_eq!(
        tax.cell(&mut tx, "amount").await.unwrap(),
        decimal("12.3456")
    );
    assert_eq!(tax.cell(&mut tx, "tax_label").await.unwrap(), text("GST"));
    tax.write(
        &mut tx,
        RawVals::new().set_cell("price_include_override", text("tax_included")),
    )
    .await
    .unwrap();
    assert_eq!(
        tax.cell(&mut tx, "price_include").await.unwrap(),
        Cell::Bool(true)
    );
    tax.write(
        &mut tx,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    let replacement = make(&taxes, &mut tx, "VAT").await;
    assert_ne!(replacement.ids(), tax.ids());
    taxes
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("VAT"))
                    .set_cell("type_tax_use", text("none")),
                RawVals::new()
                    .set_cell("name", text("VAT"))
                    .set_cell("type_tax_use", text("none")),
            ],
        )
        .await
        .unwrap();
    refused_write(
        &mut tx,
        &tax,
        RawVals::new().set_cell("active", Cell::Bool(true)),
        &duplicate("VAT", &company_name),
    )
    .await;
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

fn line(document: &str, kind: &str, percent: &str, seq: i32) -> RawVals {
    RawVals::new()
        .set_cell("document_type", text(document))
        .set_cell("repartition_type", text(kind))
        .set_cell("factor_percent", decimal(percent))
        .set_cell("sequence", Cell::Int(seq))
}
fn distribution(parts: &[(&str, &str)]) -> RawVals {
    let mut commands = vec![Command::clear()];
    for document in ["invoice", "refund"] {
        for (seq, (kind, percent)) in parts.iter().enumerate() {
            commands.push(Command::create(line(
                document,
                kind,
                percent,
                i32::try_from(seq).unwrap(),
            )));
        }
    }
    RawVals::new().set_commands("repartition_line_ids", commands)
}
#[test]
fn tax_distribution_validation_and_atomicity() {
    big_stack(distributions);
}
async fn distributions() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, _) = fixture(&mut tx).await;
    let tax = make(&taxes, &mut tx, "Split VAT").await;
    for (parts, message) in [
        (
            vec![("tax", "100")],
            "Each distribution, for invoices and for credit notes, needs exactly one base line.",
        ),
        (
            vec![("base", "100")],
            "Each distribution, for invoices and for credit notes, needs at least one tax line.",
        ),
        (
            vec![("base", "100"), ("tax", "90")],
            "The positive tax lines of each distribution must add up to 100%.",
        ),
        (
            vec![("base", "100"), ("tax", "100"), ("tax", "-80")],
            "The negative tax lines of each distribution must add up to -100%, or there must be none.",
        ),
    ] {
        refused_write(&mut tx, &tax, distribution(&parts), message).await;
    }
    assert_eq!(
        tax.related(&mut tx, "repartition_line_ids")
            .await
            .unwrap()
            .len(),
        4
    );
    let asymmetric = RawVals::new().set_commands(
        "repartition_line_ids",
        [Command::create(line("refund", "tax", "0", 2))],
    );
    refused_write(
        &mut tx,
        &tax,
        asymmetric,
        "The credit note distribution must have as many lines as the invoice distribution.",
    )
    .await;
    let mismatched_types = RawVals::new().set_commands(
        "repartition_line_ids",
        [
            Command::clear(),
            Command::create(line("invoice", "base", "100", 0)),
            Command::create(line("invoice", "tax", "100", 1)),
            Command::create(line("refund", "tax", "100", 0)),
            Command::create(line("refund", "base", "100", 1)),
        ],
    );
    refused_write(
        &mut tx,
        &tax,
        mismatched_types,
        "The credit note distribution must repeat the invoice distribution line by line: the same line types and percentages, in the same order.",
    )
    .await;
    tax.write(&mut tx, distribution(&[("base", "100"), ("tax", "99.5")]))
        .await
        .unwrap();
    refused_write(
        &mut tx,
        &tax,
        distribution(&[("base", "100"), ("tax", "99.49")]),
        "The positive tax lines of each distribution must add up to 100%.",
    )
    .await;
    tax.write(
        &mut tx,
        RawVals::new().set_cell("type_tax_use", text("purchase")),
    )
    .await
    .unwrap();
    tax.write(
        &mut tx,
        distribution(&[("base", "100"), ("tax", "80"), ("tax", "20")]),
    )
    .await
    .unwrap();
    assert_eq!(
        tax.cell(&mut tx, "non_deductible_amount").await.unwrap(),
        decimal("1")
    );
    let lines = tax.related(&mut tx, "repartition_line_ids").await.unwrap();
    for line in lines.iter() {
        if line.cell(&mut tx, "repartition_type").await.unwrap() == text("tax")
            && line.cell(&mut tx, "factor_percent").await.unwrap() == decimal("80")
        {
            line.write(
                &mut tx,
                RawVals::new().set_cell("use_in_tax_closing", Cell::Bool(true)),
            )
            .await
            .unwrap();
        }
    }
    assert_eq!(
        tax.cell(&mut tx, "non_deductible_amount").await.unwrap(),
        decimal("0.2")
    );
    let inv = lines
        .filtered_domain(
            &mut tx,
            &Domain::condition("document_type", Operator::Eq, "invoice").unwrap(),
        )
        .await
        .unwrap();
    let first = inv.browse([inv.ids()[0]]).unwrap();
    refused_write(
        &mut tx,
        &first,
        RawVals::new().set_cell("factor_percent", decimal("50")),
        "The credit note distribution must repeat the invoice distribution line by line: the same line types and percentages, in the same order.",
    )
    .await;
    tax.write(
        &mut tx,
        distribution(&[("base", "100"), ("tax", "100"), ("tax", "-100")]),
    )
    .await
    .unwrap();
    assert_eq!(
        tax.cell(&mut tx, "has_negative_factor").await.unwrap(),
        Cell::Bool(true)
    );
    let tax_id = tax.ids()[0];
    let children = tax.env().records("account.tax.repartition.line").unwrap();
    let child_model = children.clone();
    let e = tx
        .savepoint(async move |tx| {
            child_model
                .create(
                    tx,
                    vec![
                        line("invoice", "tax", "0", 99)
                            .set_cell("tax_id", Cell::Id(tax_id))
                            .set_cell("company_id", Cell::Id(1)),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "A repartition line's company is derived from its tax."
    );
    let e = tx
        .savepoint(async move |tx| {
            children
                .create(tx, vec![line("invoice", "tax", "0", 99)])
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "A tax repartition line requires an owning tax."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn tax_repartition_setting_is_bounded() {
    big_stack(setting);
}
async fn setting() {
    use erp_addon_base::ir_config_setting::{read_values, write_values};
    use erp_sdk::views::wire::SettingsScope;
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let key = "account.tax_repartition_lines_max";
    let values = read_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        vec![key.to_owned()],
    )
    .await
    .unwrap();
    let values = erp_sdk::serde_json::to_value(values).unwrap();
    assert_eq!(values["values"][0]["value"], 16);
    for invalid in [0, 1, 65] {
        let revision = values["revision"].as_str().unwrap().to_owned();
        let env = env.clone();
        let e = tx
            .savepoint(async move |tx| {
                write_values(
                    &env,
                    tx,
                    SettingsScope::Global {},
                    revision,
                    vec![(key.to_owned(), Value::Int(invalid))],
                )
                .await
            })
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(
            e.message(),
            "account.tax_repartition_lines_max takes a whole number from 2 to 64."
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn tax_company_access_and_hidden_duplicate() {
    big_stack(access);
}
async fn access() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, root) = fixture(&mut setup).await;
    let env = taxes.env().clone();
    let branch = env
        .records("res.company")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", text("Branch taxes"))
                    .set_cell("parent_id", Cell::Id(root)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let country = env
        .records("res.company")
        .unwrap()
        .browse([root])
        .unwrap()
        .cell(&mut setup, "account_fiscal_country_id")
        .await
        .unwrap();
    env.records("res.company")
        .unwrap()
        .browse([branch])
        .unwrap()
        .write(
            &mut setup,
            RawVals::new().set_cell("account_fiscal_country_id", country),
        )
        .await
        .unwrap();
    let branch_env = env.with_company(branch).unwrap();
    let hidden = branch_env
        .records("account.tax")
        .unwrap()
        .create(
            &mut setup,
            vec![RawVals::new().set_cell("name", text("Hidden VAT"))],
        )
        .await
        .unwrap();
    let manager=setup.query("SELECT res_id FROM ir_model_data WHERE module='account' AND name='group_account_manager'",&[]).await.unwrap()[0].get::<_,i64>(0);
    let internal = setup
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_user'",
            &[],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    let admin = setup
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system'",
            &[],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    env.records("res.users")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", text("Tax administrator"))
                    .set_cell("login", text("tax-admin"))
                    .set_cell("company_id", Cell::Id(root))
                    .set_commands(
                        "company_ids",
                        [Command::set([RecId::from_real(root).unwrap()])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::set([RecId::from_real(admin).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap();
    let mut users = Vec::new();
    for (login, group) in [("tax-manager", manager), ("tax-reader", internal)] {
        let id = env
            .records("res.users")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", text(login))
                        .set_cell("login", text(login))
                        .set_cell("company_id", Cell::Id(root))
                        .set_commands(
                            "company_ids",
                            [Command::set(vec![RecId::from_real(root).unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::set(vec![RecId::from_real(group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        users.push(id);
    }
    let shared = make(&taxes, &mut setup, "Root visible VAT").await;
    let root_lines = shared
        .related(&mut setup, "repartition_line_ids")
        .await
        .unwrap()
        .ids();
    let branch_user = env
        .records("res.users")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", text("tax-branch-reader"))
                    .set_cell("login", text("tax-branch-reader"))
                    .set_cell("company_id", Cell::Id(branch))
                    .set_commands(
                        "company_ids",
                        [Command::set([RecId::from_real(branch).unwrap()])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::set([RecId::from_real(internal).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    setup.commit().await.unwrap();
    drop(setup);
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
    let mut tx = secured
        .begin(erp_sdk::UserId::new(users[0]).unwrap())
        .await
        .unwrap();
    let env = tx.env().with_company(root).unwrap();
    let taxes = env.records("account.tax").unwrap();
    let e = taxes
        .browse(hidden.ids())
        .unwrap()
        .cell(&mut tx, "name")
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Access);
    assert_eq!(
        e.message(),
        format!(
            "Access rules protect some of these records.\n\ntax-manager (id={}) has no 'read' access to:\n- Tax (account.tax)\n\nAsk an administrator if you need access to these records.\n\nThe record belongs to a company outside your allowed companies.",
            users[0]
        )
    );
    let model = taxes.clone();
    let e = tx
        .savepoint(async move |tx| {
            model
                .create(
                    tx,
                    vec![RawVals::new().set_cell("name", text("Hidden VAT"))],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.message(), duplicate("Hidden VAT", "Branch taxes"));
    let permitted = make(&taxes, &mut tx, "Manager VAT").await;
    assert_eq!(
        permitted
            .related(&mut tx, "repartition_line_ids")
            .await
            .unwrap()
            .len(),
        4
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = secured
        .begin(erp_sdk::UserId::new(users[1]).unwrap())
        .await
        .unwrap();
    let taxes = tx
        .env()
        .with_company(root)
        .unwrap()
        .records("account.tax")
        .unwrap();
    let e = taxes
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", text("Denied VAT"))],
        )
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Access);
    assert_eq!(
        e.message(),
        "Your access rights do not let you create 'Tax' (account.tax) records.\n\nGroups that may do this:\n\t- Accounting Administrator\n\t- Settings Administration\n\nAsk an administrator if you need this access."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut branch_tx = secured
        .begin(erp_sdk::UserId::new(branch_user).unwrap())
        .await
        .unwrap();
    let branch_env = branch_tx.env().with_company(branch).unwrap();
    let lines = branch_env
        .records("account.tax.repartition.line")
        .unwrap()
        .browse(root_lines)
        .unwrap();
    assert_eq!(
        lines
            .cells(&mut branch_tx, "document_type")
            .await
            .unwrap()
            .len(),
        4
    );
    branch_tx.rollback().await.unwrap();
    drop(branch_tx);
    drop(secured);
    drop(orm);
    db.cleanup().await.unwrap();
}

mod used_probe {
    use erp_addon_account::tax::AccountTax;
    use erp_sdk::prelude::*;
    erp_sdk::addon!(
        name = "test_tax_used",
        depends = ["account"],
        exact_only = true
    );
    #[extend(AccountTax, addon = "test_tax_used")]
    pub struct UsedTax {}
    #[methods(AccountTax, addon = "test_tax_used")]
    impl UsedTax {
        #[method(override)]
        async fn _is_used(
            this: &RecordSet<AccountTax>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<Vec<bool>> {
            let mut used: Vec<bool> = sup.call(tx, this.as_records(), ()).await?;
            let names = this.as_records().cells(tx, "name").await?;
            for (value, name) in used.iter_mut().zip(names) {
                *value |= name == Cell::Str("Used tax fixture".into());
            }
            Ok(used)
        }
    }
}
#[test]
fn tax_delete_guard_calls_usage_extension() {
    big_stack(used);
}
async fn used() {
    let (db, orm) = install_with_used(true).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, _) = fixture(&mut tx).await;
    let tax = make(&taxes, &mut tx, "Used tax fixture").await;
    let copy = tax.clone();
    let e = tx
        .savepoint(async move |tx| copy.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "A tax that is in use cannot be deleted: archive it instead."
    );
    assert_eq!(
        tax.cell(&mut tx, "name").await.unwrap(),
        text("Used tax fixture")
    );
    let unused = make(&taxes, &mut tx, "Unused tax fixture").await;
    unused.unlink(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn concurrent_tax_creates_are_stopped_by_database_index() {
    big_stack(concurrent_unique);
}
async fn concurrent_unique() {
    let (db, orm) = install().await;
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (_, company) = fixture(&mut setup).await;
    setup.commit().await.unwrap();
    drop(setup);
    let mut first = orm.begin(SUPERUSER_ID).await.unwrap();
    let node_b = independent_node(&db, &orm);
    let mut second = node_b.begin(SUPERUSER_ID).await.unwrap();
    let second_pid: i32 = second.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    let taxes = first
        .env()
        .with_company(company)
        .unwrap()
        .records("account.tax")
        .unwrap();
    make(&taxes, &mut first, "Race VAT").await;
    let model = second
        .env()
        .with_company(company)
        .unwrap()
        .records("account.tax")
        .unwrap();
    let pending = tokio::spawn(async move {
        let result = model
            .create(
                &mut second,
                vec![RawVals::new().set_cell("name", text("Race VAT"))],
            )
            .await;
        second.rollback().await.unwrap();
        result
    });
    // Observe the competing backend waiting on the uncommitted key, then release it. No sleep
    // or fixed latency assertion decides whether this exercised an actual concurrent insert.
    let observer = db.client().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(60),async {
  loop {
   let waiting: bool=observer.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock')",&[&second_pid]).await.unwrap().get(0);
   if waiting {break;} assert!(!pending.is_finished(),"competing insert did not reach the unique-index wait");
   tokio::task::yield_now().await;
  }
 }).await.unwrap();
    first.commit().await.unwrap();
    drop(first);
    drop(observer);
    let error = pending.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!("The database refused the change: {DUPLICATE}")
    );
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = check
        .query(
            "SELECT count(*)::int8 FROM account_tax WHERE name->>'en_US'='Race VAT'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 1);
    check.rollback().await.unwrap();
    drop(check);
    drop(node_b);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn tax_group_country_and_price_inclusion_onchange() {
    big_stack(country_and_onchange);
}
async fn country_and_onchange() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, company) = fixture(&mut tx).await;
    let tax = make(&taxes, &mut tx, "Country VAT").await;
    let other = tx
        .query("SELECT id FROM res_country WHERE code='DE'", &[])
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    refused_write(
        &mut tx,
        &tax,
        RawVals::new().set_cell("country_id", Cell::Id(other)),
        "The tax group belongs to another country than the tax: choose a tax group of the tax's country, or one without a country.",
    )
    .await;
    let fields = erp_sdk::ValueMap::from_iter(
        [
            "price_include_override",
            "price_include",
            "include_base_amount",
            "company_id",
        ]
        .into_iter()
        .map(|key| {
            (
                key.to_owned(),
                Value::Map(Arc::new(erp_sdk::ValueMap::new())),
            )
        }),
    );
    let spec = erp_orm::Spec::from_value(&Value::Map(Arc::new(fields))).unwrap();
    let values = erp_sdk::ValueMap::from_iter([
        (
            "price_include_override".to_owned(),
            Value::str("tax_included"),
        ),
        ("include_base_amount".to_owned(), Value::Bool(false)),
        ("company_id".to_owned(), Value::Int(company)),
    ]);
    let result = taxes
        .onchange(
            &mut tx,
            values,
            vec!["price_include_override".to_owned()],
            spec,
        )
        .await
        .unwrap();
    let json = erp_fields::convert::value_to_json(&result).unwrap();
    assert_eq!(json["value"]["include_base_amount"], true);
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn installed_tax_views_are_served_with_model_fields() {
    big_stack(served_views);
}
async fn served_views() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let company = main_company(&mut tx).await;
    let env = tx.env().with_company(company).unwrap();
    let mut descriptions = erp_sdk::serde_json::Map::new();
    for (model, expected) in [
        (
            "account.tax",
            vec![
                "name",
                "amount",
                "invoice_repartition_line_ids",
                "refund_repartition_line_ids",
            ],
        ),
        (
            "account.tax.group",
            vec!["name", "country_id", "preceding_subtotal"],
        ),
        (
            "account.tax.repartition.line",
            vec![
                "tax_id",
                "document_type",
                "repartition_type",
                "factor_percent",
                "account_id",
                "tag_ids",
                "use_in_tax_closing",
            ],
        ),
        (
            "res.company",
            vec!["account_fiscal_country_id", "account_price_include"],
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
        .unwrap();
        let json = erp_sdk::serde_json::to_value(views).unwrap();
        descriptions.insert(model.to_owned(), json.clone());
        if model == "account.tax" {
            // a new line of either grid gets its document side from the field's context
            assert!(json.to_string().contains("default_document_type"), "{json}");
        }
        assert_eq!(json["views"].as_array().unwrap().len(), 2);
        let fields = json["fields"][model].as_object().unwrap();
        for field in expected {
            assert!(fields.contains_key(field), "{model}.{field}");
        }
        assert!(
            json["views"]
                .as_array()
                .unwrap()
                .iter()
                .all(|view| view["identity"]["id"].as_i64().is_some_and(|id| id > 0)),
            "{json}"
        );
    }
    std::fs::write(
        "/tmp/a later release",
        erp_sdk::serde_json::to_vec(&descriptions).unwrap(),
    )
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn source_names_remain_unique_across_translations() {
    big_stack(translations);
}
async fn translations() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let langs = tx
        .env()
        .with_context([("active_test", Value::Bool(false))])
        .records("res.lang")
        .unwrap()
        .search(
            &mut tx,
            &Domain::condition("code", Operator::Eq, "fr_FR").unwrap(),
        )
        .await
        .unwrap();
    langs
        .write(&mut tx, RawVals::new().set_cell("active", Cell::Bool(true)))
        .await
        .unwrap();
    let (taxes, company) = fixture(&mut tx).await;
    let first = make(&taxes, &mut tx, "Source VAT").await;
    let french = taxes
        .env()
        .with_context([("lang", Value::str("fr_FR"))])
        .records("account.tax")
        .unwrap();
    french
        .browse(first.ids())
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", text("Nom traduit")),
        )
        .await
        .unwrap();
    assert_eq!(
        first
            .get_field_translations(&mut tx, "name")
            .await
            .unwrap()
            .get("en_US"),
        Some("Source VAT")
    );
    assert_eq!(
        first.cell(&mut tx, "tax_label").await.unwrap(),
        text("Source VAT")
    );
    assert_eq!(
        french
            .browse(first.ids())
            .unwrap()
            .cell(&mut tx, "tax_label")
            .await
            .unwrap(),
        text("Nom traduit")
    );
    let company_name = taxes
        .env()
        .records("res.company")
        .unwrap()
        .browse([company])
        .unwrap()
        .cell(&mut tx, "name")
        .await
        .unwrap();
    let Cell::Str(company_name) = company_name else {
        panic!("company name")
    };
    let name = Cell::Json(Arc::new(
        erp_sdk::serde_json::json!({"en_US":"Source VAT","fr_FR":"Autre traduction"}),
    ));
    let model = french.clone();
    let e = tx
        .savepoint(async move |tx| {
            model
                .create(tx, vec![RawVals::new().set_cell("name", name)])
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.message(), duplicate("Source VAT", &company_name));
    let second = make(&taxes, &mut tx, "Different source VAT").await;
    french
        .browse(second.ids())
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", text("Nom traduit")),
        )
        .await
        .unwrap();
    assert_eq!(
        second
            .get_field_translations(&mut tx, "name")
            .await
            .unwrap()
            .get("en_US"),
        Some("Different source VAT")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn repartition_count_uses_the_current_database_setting() {
    big_stack(count_bound);
}
async fn set_limit(tx: &mut Transaction, n: i64) {
    use erp_addon_base::ir_config_setting::{read_values, write_values};
    use erp_sdk::views::wire::SettingsScope;
    let env = tx.env();
    let key = "account.tax_repartition_lines_max";
    let value = read_values(&env, tx, SettingsScope::Global {}, vec![key.to_owned()])
        .await
        .unwrap();
    let value = erp_sdk::serde_json::to_value(value).unwrap();
    write_values(
        &env,
        tx,
        SettingsScope::Global {},
        value["revision"].as_str().unwrap().to_owned(),
        vec![(key.to_owned(), Value::Int(n))],
    )
    .await
    .unwrap();
}
async fn count_bound() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, _) = fixture(&mut tx).await;
    set_limit(&mut tx, 2).await;
    let tax = make(&taxes, &mut tx, "Bounded VAT").await;
    refused_write(&mut tx,&tax,distribution(&[("base","100"),("tax","50"),("tax","50")]),"Tax distribution exceeds 2 lines per document kind (setting account.tax_repartition_lines_max).").await;
    set_limit(&mut tx, 64).await;
    let mut parts = vec![("base", "100"), ("tax", "100")];
    parts.extend(std::iter::repeat_n(("tax", "0"), 62));
    tax.write(&mut tx, distribution(&parts)).await.unwrap();
    assert_eq!(
        tax.related(&mut tx, "repartition_line_ids")
            .await
            .unwrap()
            .len(),
        128
    );
    parts.push(("tax", "0"));
    refused_write(&mut tx,&tax,distribution(&parts),"Tax distribution exceeds 64 lines per document kind (setting account.tax_repartition_lines_max).").await;
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn tax_batch_duplicates_are_refused_without_partial_rows() {
    big_stack(batch_names);
}
async fn batch_names() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, company) = fixture(&mut tx).await;
    let company_name = taxes
        .env()
        .records("res.company")
        .unwrap()
        .browse([company])
        .unwrap()
        .cell(&mut tx, "name")
        .await
        .unwrap();
    let Cell::Str(company_name) = company_name else {
        panic!("company name")
    };
    let model = taxes.clone();
    let e = tx
        .savepoint(async move |tx| {
            model
                .create(
                    tx,
                    vec![
                        RawVals::new().set_cell("name", text("Batch VAT")),
                        RawVals::new().set_cell("name", text("Batch VAT")),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.message(), duplicate("Batch VAT", &company_name));
    assert_eq!(
        taxes
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        0
    );
    let first = taxes
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Scoped VAT"))
                    .set_cell("tax_scope", text("service")),
            ],
        )
        .await
        .unwrap();
    let second = taxes
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Scoped VAT"))
                    .set_cell("tax_scope", text("consu")),
            ],
        )
        .await
        .unwrap();
    let both = first.union(&second).unwrap();
    refused_write(
        &mut tx,
        &both,
        RawVals::new().set_cell("tax_scope", Cell::Null),
        &duplicate("Scoped VAT", &company_name),
    )
    .await;
    assert_eq!(
        first.cell(&mut tx, "tax_scope").await.unwrap(),
        text("service")
    );
    assert_eq!(
        second.cell(&mut tx, "tax_scope").await.unwrap(),
        text("consu")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn one_call_creates_taxes_of_one_name_for_two_uses() {
    big_stack(two_uses);
}
async fn two_uses() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, _) = fixture(&mut tx).await;
    let pair = taxes
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("15%"))
                    .set_cell("type_tax_use", text("sale")),
                RawVals::new()
                    .set_cell("name", text("15%"))
                    .set_cell("type_tax_use", text("purchase")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(pair.len(), 2);
    // a field outside the key, then the key's active flag, on both at once
    pair.write(&mut tx, RawVals::new().set_cell("sequence", Cell::Int(7)))
        .await
        .unwrap();
    pair.write(
        &mut tx,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    pair.write(&mut tx, RawVals::new().set_cell("active", Cell::Bool(true)))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn a_company_change_moves_only_the_root() {
    big_stack(company_change);
}
async fn company_change() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, main) = fixture(&mut tx).await;
    let env = taxes.env().clone();
    let companies = env.records("res.company").unwrap();
    let currency = companies
        .browse([main])
        .unwrap()
        .cell(&mut tx, "currency_id")
        .await
        .unwrap();
    let branch = companies
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Tax branch"))
                    .set_cell("parent_id", Cell::Id(main)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let france = id_of(&mut tx, "SELECT id FROM res_country WHERE code = 'FR'").await;
    companies
        .browse([branch])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("account_fiscal_country_id", Cell::Id(france)),
        )
        .await
        .unwrap();
    let other = companies
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Other tax root"))
                    .set_cell("currency_id", currency),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    // a group after the fixture's group in the order, so a re-derived group would differ
    let chosen = env
        .records("account.tax.group")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Chosen group"))
                    .set_cell("sequence", Cell::Int(50)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let other_group = env
        .with_company(other)
        .unwrap()
        .records("account.tax.group")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", text("Other root group"))],
        )
        .await
        .unwrap()
        .ids()[0];
    let tax = taxes
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Moved VAT"))
                    .set_cell("tax_group_id", Cell::Id(chosen)),
            ],
        )
        .await
        .unwrap();
    let country = tax.cell(&mut tx, "country_id").await.unwrap();
    assert!(matches!(country, Cell::Id(_)), "{country:?}");
    // inside the tree: same root, and country and group stay
    tax.write(
        &mut tx,
        RawVals::new().set_cell("company_id", Cell::Id(branch)),
    )
    .await
    .unwrap();
    assert_eq!(
        tax.cell(&mut tx, "root_company_id").await.unwrap(),
        Cell::Id(main)
    );
    assert_eq!(tax.cell(&mut tx, "country_id").await.unwrap(), country);
    assert_eq!(
        tax.cell(&mut tx, "tax_group_id").await.unwrap(),
        Cell::Id(chosen)
    );
    // into another tree: the root follows the company
    tax.write(
        &mut tx,
        RawVals::new()
            .set_cell("company_id", Cell::Id(other))
            .set_cell("tax_group_id", Cell::Id(other_group)),
    )
    .await
    .unwrap();
    assert_eq!(
        tax.cell(&mut tx, "root_company_id").await.unwrap(),
        Cell::Id(other)
    );
    assert_eq!(tax.cell(&mut tx, "country_id").await.unwrap(), country);
    // server code that writes the company past the tax's own write: the root follows too
    tax.write_base(
        &mut tx,
        RawVals::new()
            .set_cell("company_id", Cell::Id(main))
            .set_cell("tax_group_id", Cell::Id(chosen)),
    )
    .await
    .unwrap();
    assert_eq!(
        tax.cell(&mut tx, "root_company_id").await.unwrap(),
        Cell::Id(main)
    );
    tx.flush().await.unwrap();
    let stored: i64 = tx
        .query(
            "SELECT root_company_id::int8 FROM account_tax WHERE id = $1",
            &[&tax.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, main);
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

fn independent_node(db: &TestDb, orm: &Orm) -> Orm {
    Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}
#[test]
fn concurrent_distribution_edits_serialize_across_nodes() {
    big_stack(concurrent_edits);
}
async fn concurrent_edits() {
    let (db, orm) = install().await;
    let node_b = independent_node(&db, &orm);
    let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, company) = fixture(&mut setup).await;
    let tax = make(&taxes, &mut setup, "Concurrent split VAT").await;
    let id = tax.ids()[0];
    setup.commit().await.unwrap();
    drop(setup);
    let mut first = orm.begin(SUPERUSER_ID).await.unwrap();
    let mut second = node_b.begin(SUPERUSER_ID).await.unwrap();
    let pid: i32 = second.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    let model = first
        .env()
        .with_company(company)
        .unwrap()
        .records("account.tax")
        .unwrap()
        .browse([id])
        .unwrap();
    model
        .write(
            &mut first,
            distribution(&[("base", "100"), ("tax", "50"), ("tax", "50")]),
        )
        .await
        .unwrap();
    let model = second
        .env()
        .with_company(company)
        .unwrap()
        .records("account.tax")
        .unwrap()
        .browse([id])
        .unwrap();
    let pending = tokio::spawn(async move {
        let result = model
            .write(
                &mut second,
                distribution(&[("base", "100"), ("tax", "30"), ("tax", "70")]),
            )
            .await;
        second.rollback().await.unwrap();
        result
    });
    let observer = db.client().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(60),async {loop {
  let waiting:bool=observer.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock')",&[&pid]).await.unwrap().get(0);
  if waiting {break;}assert!(!pending.is_finished(),"second edit must wait for the first parent row version");tokio::task::yield_now().await;
 }}).await.unwrap();
    first.commit().await.unwrap();
    drop(first);
    drop(observer);
    let error = pending.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency);
    assert_eq!(error.sqlstate(), Some("40001"));
    // The database's localized serialization text is not stable; SQLSTATE and error kind are the contract.
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows=check.query("SELECT factor_percent::text FROM account_tax_repartition_line WHERE tax_id=$1 AND document_type='invoice' AND repartition_type='tax' ORDER BY sequence,id",&[&id]).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>(),
        ["50", "50"]
    );
    check.rollback().await.unwrap();
    drop(check);
    drop(node_b);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn moving_existing_lines_fences_and_validates_both_parents() {
    big_stack(moving_lines);
}
async fn moving_lines() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, _) = fixture(&mut tx).await;
    let source = make(&taxes, &mut tx, "Source distribution").await;
    source
        .write(
            &mut tx,
            distribution(&[("base", "100"), ("tax", "100"), ("tax", "0")]),
        )
        .await
        .unwrap();
    let target = make(&taxes, &mut tx, "Target distribution").await;
    let source_id = source.ids()[0];
    let before: i32 = tx
        .query(
            "SELECT repartition_serial FROM account_tax WHERE id=$1",
            &[&source_id],
        )
        .await
        .unwrap()[0]
        .get(0);
    let zeros = source
        .related(&mut tx, "repartition_line_ids")
        .await
        .unwrap()
        .filtered_domain(
            &mut tx,
            &Domain::condition("factor_percent", Operator::Eq, 0).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(zeros.len(), 2);
    target
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "repartition_line_ids",
                zeros.rec_ids().iter().copied().map(Command::Link),
            ),
        )
        .await
        .unwrap();
    let after: i32 = tx
        .query(
            "SELECT repartition_serial FROM account_tax WHERE id=$1",
            &[&source_id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(
        after > before,
        "the nested source parent must receive a row-version fence"
    );
    assert_eq!(
        source
            .related(&mut tx, "repartition_line_ids")
            .await
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        target
            .related(&mut tx, "repartition_line_ids")
            .await
            .unwrap()
            .len(),
        6
    );
    let invoice = source
        .related(&mut tx, "invoice_repartition_line_ids")
        .await
        .unwrap();
    let base = invoice
        .filtered_domain(
            &mut tx,
            &Domain::condition("repartition_type", Operator::Eq, "base").unwrap(),
        )
        .await
        .unwrap();
    refused_write(
        &mut tx,
        &target,
        RawVals::new().set_commands("repartition_line_ids", [Command::Link(base.rec_ids()[0])]),
        "Each distribution, for invoices and for credit notes, needs exactly one base line.",
    )
    .await;
    assert_eq!(
        source
            .related(&mut tx, "repartition_line_ids")
            .await
            .unwrap()
            .len(),
        4
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

#[test]
fn upgrading_account_preserves_existing_tax_configuration() {
    big_stack(upgrade_existing);
}
async fn upgrade_existing() {
    let (db, orm) = install().await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let (taxes, company) = fixture(&mut tx).await;
    let tax = make(&taxes, &mut tx, "Retained tax through upgrade").await;
    tax.write(
        &mut tx,
        RawVals::new().set_cell("amount", decimal("12.3456")),
    )
    .await
    .unwrap();
    let tax_id = tax.ids()[0];
    let before = tax
        .related(&mut tx, "repartition_line_ids")
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);
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
    let outcome = Box::pin(system.button_immediate_upgrade(
        &system.system_env(Context::new()),
        &[ModuleName::new("account").unwrap()],
    ))
    .await
    .unwrap();
    assert!(outcome.post_commit.is_clean(), "{:?}", outcome.post_commit);
    let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
    let tax = tx
        .env()
        .with_company(company)
        .unwrap()
        .records("account.tax")
        .unwrap()
        .browse([tax_id])
        .unwrap();
    assert_eq!(
        tax.cell(&mut tx, "name").await.unwrap(),
        text("Retained tax through upgrade")
    );
    assert_eq!(
        tax.cell(&mut tx, "amount").await.unwrap(),
        decimal("12.3456")
    );
    assert_eq!(
        tax.related(&mut tx, "repartition_line_ids")
            .await
            .unwrap()
            .ids(),
        before
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(outcome);
    drop(system);
    db.cleanup().await.unwrap();
}
