// SPDX-License-Identifier: MIT OR Apache-2.0
//! Declared technical parameters of two addons (base and uom) through an initialized database:
//! default rows, Technical write checks, the prefix rule, groups and typed reads.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
use erp_addon_base::ADDON;
use erp_sdk::prelude::*;
use erp_sdk::settings::{ParameterDecl, SettingGroupDecl};
use erp_sdk::{ErrorKind, SUPERUSER_ID};
use std::collections::BTreeMap;

// Declared as base: `crate::ADDON` of this test crate is base's descriptor.
fn base_parameters() -> Result<Vec<ParameterDecl>> {
    Ok(vec![
        ParameterDecl::integer("base.test_param_batch", "Test batch", 50, (1, 100))?,
        ParameterDecl::boolean("base.test_param_flag", "Test flag", true)?,
    ])
}
erp_sdk::system_parameters!(base_parameters);

// Declared as uom, the second addon, which depends on base.
fn uom_parameters() -> Result<Vec<ParameterDecl>> {
    Ok(vec![
        ParameterDecl::integer("uom.test_low_count", "Low", 1, (1, 10))?,
        ParameterDecl::integer("uom.test_high_count", "High", 10, (1, 10))?,
        ParameterDecl::float("uom.test_ratio", "Ratio", 1.5, (0.5, 2.0))?,
        ParameterDecl::selection(
            "uom.test_mode",
            "Mode",
            "exact",
            &[("exact", "Exact"), ("rounded", "Rounded")],
        )?,
        ParameterDecl::string("uom.test_api_token", "Provider token", "tok-default", 256)?.secret(),
    ])
}
fn ordered(values: &BTreeMap<String, Value>) -> Result<()> {
    match (
        &values["uom.test_low_count"],
        &values["uom.test_high_count"],
    ) {
        (Value::Int(low), Value::Int(high)) if low <= high => Ok(()),
        _ => Err(Error::validation(
            "uom.test_low_count must not exceed uom.test_high_count.",
        )),
    }
}
fn uom_groups() -> Result<Vec<SettingGroupDecl>> {
    Ok(vec![SettingGroupDecl::new(
        "uom.test_counts",
        &["uom.test_low_count", "uom.test_high_count"],
        ordered,
    )?])
}
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::params::PARAMETER_CONTRIBUTIONS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static UOM_PARAMETERS: erp_sdk::params::ParameterContribution =
    erp_sdk::params::ParameterContribution::new(&erp_addon_uom::ADDON, uom_parameters, uom_groups);

async fn initialized(db: &erp_testkit::TestDb) -> erp_orm::Orm {
    let mut index = erp_modules::AddonIndex::new(true);
    index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../addons")
            .canonicalize()
            .unwrap(),
    );
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
    let system = erp_modules::system::ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(native_artifact_identities)
        .code([&ADDON, &erp_addon_uom::ADDON])
        .build()
        .unwrap();
    let init = Box::pin(
        system.initialize(erp_modules::system::InitOptions::new().with_skip_auto_install(true)),
    )
    .await
    .unwrap();
    // uom is installed by a later operation: its default rows come with that operation
    let env = init.orm.env(SUPERUSER_ID, erp_orm::Context::new());
    Box::pin(system.button_immediate_install(&env, &[erp_modules::ModuleName::new("uom").unwrap()]))
        .await
        .unwrap()
        .orm
}

async fn stored(orm: &erp_orm::Orm, key: &str) -> Option<Option<String>> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key=$1",
            &[&key],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows.first().map(|row| row.get(0))
}

/// `set_str` through the model in its own transaction, committed only when accepted.
async fn set(orm: &erp_orm::Orm, key: &str, value: Option<&str>) -> Result<()> {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let params = tx.env().records("ir.config_parameter").unwrap();
    let outcome: Result<String> = params
        .call(
            &mut tx,
            "set_str",
            (key.to_owned(), value.map(str::to_owned)),
        )
        .await;
    match outcome {
        Ok(_) => {
            tx.commit().await.unwrap();
            Ok(())
        }
        Err(error) => {
            tx.rollback().await.unwrap();
            Err(error)
        }
    }
}

fn refused(result: Result<()>, kind: ErrorKind, message: &str) {
    let error = result.unwrap_err();
    assert_eq!((error.kind(), error.message()), (kind, message));
}

#[test]
fn two_addons_declare_check_and_read_parameters() {
    // The test future is large; the default 2 MiB test-thread stack overflows under the workspace build.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(walk());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[allow(
    clippy::too_many_lines,
    reason = "one walk through an initialized database"
)]
async fn walk() {
    let db = erp_testkit::TestDb::create().await.unwrap();
    let orm = initialized(&db).await;
    default_rows(&orm).await;
    technical_writes(&orm).await;
    groups(&orm).await;
    typed_reads(&orm).await;
    declared_secret(&orm).await;
    drop(orm);
    db.cleanup().await.unwrap();
}

// Default rows of both addons.
async fn default_rows(orm: &erp_orm::Orm) {
    for (key, value) in [
        ("base.test_param_batch", "50"),
        ("base.test_param_flag", "True"),
        ("uom.test_low_count", "1"),
        ("uom.test_high_count", "10"),
        ("uom.test_ratio", "1.5"),
        ("uom.test_mode", "exact"),
        // a moved base key without an init or data row
        ("sessions.touch_interval_seconds", "60"),
    ] {
        assert_eq!(stored(orm, key).await, Some(Some(value.into())), "{key}");
    }
    // never overwritten: init stores 10 although the declared default (an empty value) is 5,
    // and the data file's row keeps its value
    assert_eq!(
        stored(orm, "base.login_cooldown_after").await,
        Some(Some("10".into()))
    );
    assert_eq!(
        stored(orm, "base.default_max_email_size").await,
        Some(Some("20".into()))
    );
    // no row for the per-login threshold: absent, it stays four times the per-address value
    assert_eq!(stored(orm, "base.login_cooldown_after_login").await, None);
}

// Technical write checks and the prefix rule.
async fn technical_writes(orm: &erp_orm::Orm) {
    // both addons' bounds, the value never echoed
    refused(
        set(orm, "base.test_param_batch", Some("101")).await,
        ErrorKind::Validation,
        "base.test_param_batch takes a whole number from 1 to 100.",
    );
    refused(
        set(orm, "uom.test_ratio", Some("abc")).await,
        ErrorKind::Validation,
        "uom.test_ratio takes a number from 0.5 to 2.",
    );
    refused(
        set(orm, "uom.test_mode", Some("loose")).await,
        ErrorKind::Validation,
        "uom.test_mode takes one of exact, rounded.",
    );
    assert_eq!(
        stored(orm, "base.test_param_batch").await,
        Some(Some("50".into()))
    );
    set(orm, "uom.test_mode", Some("rounded")).await.unwrap();
    // a moved base key keeps its existing message
    refused(
        set(orm, "base.partition_upkeep_interval_secs", Some("59")).await,
        ErrorKind::Validation,
        "base.partition_upkeep_interval_secs must be an integer from 60 to 3600.",
    );
    // a typo under a declaring prefix is refused; other keys stay free text
    for typo in ["uom.test_hihg_count", "base.test_param_btach"] {
        refused(
            set(orm, typo, Some("5")).await,
            ErrorKind::Validation,
            &format!("Parameter {typo} is not declared by an installed addon."),
        );
        assert_eq!(stored(orm, typo).await, None);
    }
    set(orm, "integrator.custom_flag", Some("anything"))
        .await
        .unwrap();
    // a rename onto a declared key is checked with the row's value
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let params = tx.env().records("ir.config_parameter").unwrap();
    let ids = tx
        .query(
            "SELECT id::int8 FROM ir_config_parameter WHERE key='integrator.custom_flag'",
            &[],
        )
        .await
        .unwrap();
    let row = params.browse([ids[0].get::<_, i64>(0)]).unwrap();
    let error = row
        .write_values(
            &mut tx,
            &BTreeMap::from([("key".to_owned(), Value::str("uom.test_typo_key"))]),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.message(),
        "Parameter uom.test_typo_key is not declared by an installed addon."
    );
    tx.rollback().await.unwrap();
}

// A group over two uom parameters.
async fn groups(orm: &erp_orm::Orm) {
    set(orm, "uom.test_low_count", Some("9")).await.unwrap();
    refused(
        set(orm, "uom.test_high_count", Some("5")).await,
        ErrorKind::Validation,
        "uom.test_low_count must not exceed uom.test_high_count.",
    );
    assert_eq!(
        stored(orm, "uom.test_high_count").await,
        Some(Some("10".into()))
    );
    // emptying the high count resets it to its default 10: accepted
    set(orm, "uom.test_high_count", None).await.unwrap();
    set(orm, "uom.test_low_count", Some("10")).await.unwrap();
}

// A declared secret joins base's closed list; superuser-mode code still reads it.
async fn declared_secret(orm: &erp_orm::Orm) {
    let secrets = erp_addon_base::ir_config_parameter::secret_keys(orm.registry()).unwrap();
    for key in ["database.secret", "database.uuid", "uom.test_api_token"] {
        assert!(secrets.iter().any(|k| k == key), "{key} in {secrets:?}");
    }
    assert!(!secrets.iter().any(|k| k == "uom.test_mode"));
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        erp_sdk::params::get_str(&mut tx, &erp_addon_uom::ADDON, "uom.test_api_token")
            .await
            .unwrap(),
        "tok-default"
    );
    tx.rollback().await.unwrap();
}

// Typed reads across the dependency closure.
async fn typed_reads(orm: &erp_orm::Orm) {
    use erp_sdk::params::{get_bool, get_float, get_int, get_str};
    let uom = &erp_addon_uom::ADDON;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    // uom reads its own keys and base's (a dependency)
    assert_eq!(
        get_int(&mut tx, uom, "base.test_param_batch")
            .await
            .unwrap(),
        50
    );
    assert!(
        get_bool(&mut tx, uom, "base.test_param_flag")
            .await
            .unwrap()
    );
    assert!((get_float(&mut tx, uom, "uom.test_ratio").await.unwrap() - 1.5).abs() < f64::EPSILON);
    assert_eq!(
        get_str(&mut tx, uom, "uom.test_mode").await.unwrap(),
        "rounded"
    );
    // the emptied high count reads as its declared default
    assert_eq!(
        get_int(&mut tx, uom, "uom.test_high_count").await.unwrap(),
        10
    );
    // base does not depend on uom
    let error = get_int(&mut tx, &ADDON, "uom.test_low_count")
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Programming,
            "Addon base reads parameter uom.test_low_count of uom, which is outside its dependencies."
        )
    );
    let error = get_bool(&mut tx, uom, "base.test_param_batch")
        .await
        .unwrap_err();
    assert_eq!(
        error.message(),
        "Parameter base.test_param_batch is not declared as a boolean."
    );
    let error = get_int(&mut tx, uom, "integrator.custom_flag")
        .await
        .unwrap_err();
    assert_eq!(
        error.message(),
        "Parameter integrator.custom_flag is not declared by an installed addon."
    );
    // stored text outside the declaration (written around the model) fails, never the default
    tx.execute(
        "UPDATE ir_config_parameter SET value='999' WHERE key='base.test_param_batch'",
        &[],
    )
    .await
    .unwrap();
    let error = get_int(&mut tx, uom, "base.test_param_batch")
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Validation,
            "Stored parameter base.test_param_batch is invalid."
        )
    );
    tx.rollback().await.unwrap();
}
