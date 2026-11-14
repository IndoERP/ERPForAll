//! Cross-addon extension end to end (§11): native addons in separate crates
//! extending one model and one method, a data-only addon's `models/*.toml`, and custom `x_` models
//! and fields applied through a per-database overlay — each against a real database.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::path::Path;
use std::sync::Arc;

use erp_addon_test_ext_a::{Book, BookMethods};
use erp_addon_test_ext_b::BookPages;
use erp_addon_test_ext_c::BookRating;
use erp_fields::{ErrorKind, Value};
use erp_modules::{ModuleName, addon_descriptor, read_manifest_from_dir, read_model_files};
use erp_orm::{
    BoxFuture, Orm, SCHEMA_LOCK_TIMEOUT, SCHEMA_STATEMENT_TIMEOUT, SUPERUSER_ID, StaticPolicy,
};
use erp_registry::{
    AddonDescriptor, Databases, DefaultValue, FieldDef, KindDef, ManualModel, Overlay, Registry,
    RegistryCache,
};
use erp_sdk::{Cell, Domain, RawVals, Transaction, Vals};
use erp_test_models::{base_users, test_rel};
use erp_testkit::{Harness, HarnessError};

/// Column names of `table`, in order.
async fn columns(tx: &mut Transaction, table: &str) -> Vec<String> {
    support::rows(
        tx,
        &format!(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_name = '{table}' ORDER BY ordinal_position"
        ),
    )
    .await
    .into_iter()
    .map(|row| row[0].clone().unwrap())
    .collect()
}

fn has(columns: &[String], name: &str) -> bool {
    columns.iter().any(|c| c == name)
}

/// Three separately compiled addon crates: A defines `test_ext.book`; B and C (both depending
/// only on A) extend it with fields and override `describe` calling `super`.
#[tokio::test]
async fn native_addons_in_separate_crates_extend_one_model() {
    let h = Harness::builder([
        &erp_addon_test_ext_a::ADDON,
        &erp_addon_test_ext_b::ADDON,
        &erp_addon_test_ext_c::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let spec = h.registry().model("test_ext.book").unwrap();
    let mro: Vec<&str> = spec.mro.iter().map(|d| d.addon).collect();
    assert_eq!(
        mro,
        ["test_ext_c", "test_ext_b", "test_ext_a", "web", "base"]
    );
    let modules = |f: &str| spec.field(f).unwrap().modules.clone();
    assert_eq!(modules("name"), ["test_ext_a"]);
    assert_eq!(modules("pages"), ["test_ext_b"]);
    assert_eq!(modules("rating"), ["test_ext_c"]);
    assert_eq!(spec.field("isbn").unwrap().string, "ISBN");
    let chain: Vec<&str> = h
        .orm()
        .dispatch()
        .implementations(spec.id, "describe")
        .iter()
        .map(|i| i.addon)
        .collect();
    assert_eq!(chain, ["test_ext_c", "test_ext_b", "test_ext_a"]);

    let mut tx = h.begin().await.unwrap();
    let cols = columns(&mut tx, "test_ext_book").await;
    for c in ["name", "pages", "isbn", "rating"] {
        assert!(has(&cols, c), "{c} missing from {cols:?}");
    }
    // typed handles of all three crates write and read the one model
    let book = tx
        .env()
        .model::<Book>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Book::NAME, "Dune")
                    .set(BookPages::PAGES, 412)
                    .set(BookPages::ISBN, "978-0441013593"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        book.get(&mut tx, BookRating::RATING).await.unwrap(),
        3,
        "C's default"
    );
    assert_eq!(book.get(&mut tx, BookPages::PAGES).await.unwrap(), 412);
    // C runs first, then B through super, then A
    assert_eq!(
        book.describe(&mut tx).await.unwrap(),
        "book Dune, 412 pages [3/5]"
    );
    tx.flush().await.unwrap();
    let stored = support::rows(
        &mut tx,
        "SELECT name, pages::text, isbn, rating::text FROM test_ext_book",
    )
    .await;
    assert_eq!(
        stored,
        vec![vec![
            Some("Dune".to_owned()),
            Some("412".to_owned()),
            Some("978-0441013593".to_owned()),
            Some("3".to_owned()),
        ]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// All three crates are linked into this test binary, but only installed addons contribute:
/// without C there is no `rating` field, column or override.
#[tokio::test]
async fn linked_but_not_installed_addons_contribute_nothing() {
    let h = Harness::builder([&erp_addon_test_ext_a::ADDON, &erp_addon_test_ext_b::ADDON])
        .build()
        .await
        .unwrap();
    let spec = h.registry().model("test_ext.book").unwrap();
    assert!(spec.field("rating").is_none());
    assert!(spec.field("pages").is_some());
    assert!(!h.registry().addons().iter().any(|a| a.name == "test_ext_c"));
    let mut tx = h.begin().await.unwrap();
    assert!(!has(&columns(&mut tx, "test_ext_book").await, "rating"));
    let book = tx
        .env()
        .model::<Book>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Book::NAME, "Emma")
                    .set(BookPages::PAGES, 300),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        book.describe(&mut tx).await.unwrap(),
        "book Emma, 300 pages"
    );
    // C's typed handle compiles (the crate is linked) but names no field of this registry
    let err = book.get(&mut tx, BookRating::RATING).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "Invalid field 'rating' on 'test_ext.book'");
    drop(tx);
    h.cleanup().await.unwrap();

    // only A: the plain method
    let h = Harness::builder([&erp_addon_test_ext_a::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let book = tx
        .env()
        .model::<Book>()
        .unwrap()
        .create(&mut tx, vec![Vals::new().set(Book::NAME, "Ulysses")])
        .await
        .unwrap();
    assert_eq!(book.describe(&mut tx).await.unwrap(), "book Ulysses");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The fixture crates' `manifest.toml` and their `addon!()` agree on name and dependencies.
#[test]
fn fixture_manifests_match_addon_descriptors() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("addons");
    for addon in [
        &erp_addon_test_ext_a::ADDON,
        &erp_addon_test_ext_b::ADDON,
        &erp_addon_test_ext_c::ADDON,
    ] {
        let name = ModuleName::new(addon.name).unwrap();
        let manifest = read_manifest_from_dir(&root.join(addon.name), &name)
            .unwrap()
            .unwrap();
        let from_manifest = addon_descriptor(&manifest).unwrap();
        assert_eq!(
            (from_manifest.name, from_manifest.depends),
            (addon.name, addon.depends)
        );
    }
}

/// creates its table and column, and its records are written and read through the ORM. The
/// `installed` state and data files arrive with)
#[tokio::test]
async fn spec_mod_045_data_only_addon_tables_and_rows() {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../erp-modules/tests/fixtures/test_data_only");
    let name = ModuleName::new("test_data_only").unwrap();
    let manifest = read_manifest_from_dir(&dir, &name).unwrap().unwrap();
    let addon = addon_descriptor(&manifest).unwrap();
    let mut overlay = Overlay::new();
    overlay
        .add_addon_defs(read_model_files(&dir, &name).unwrap())
        .unwrap();
    let h = Harness::builder([addon])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let tag_cols = columns(&mut tx, "test_data_only_tag").await;
    for c in ["id", "name", "sequence", "color", "create_uid"] {
        assert!(has(&tag_cols, c), "{c} missing from {tag_cols:?}");
    }
    let user_cols = columns(&mut tx, "res_users").await;
    assert!(has(&user_cols, "tag_id"));
    assert!(
        !has(&user_cols, "tag_name"),
        "a related field has no column"
    );

    let tags = tx.env().records("test_data_only.tag").unwrap();
    let tag = tags
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Gold".into()))],
        )
        .await
        .unwrap();
    // static defaults from the model file
    assert_eq!(
        tag.value(&mut tx, "sequence").await.unwrap(),
        Value::Int(10)
    );
    assert_eq!(
        tag.value(&mut tx, "color").await.unwrap(),
        Value::str("red")
    );
    let users = tx.env().records("res.users").unwrap().browse([1]).unwrap();
    users
        .set_value(&mut tx, "tag_id", Value::Int(tag.ids()[0]))
        .await
        .unwrap();
    assert_eq!(
        users.value(&mut tx, "tag_name").await.unwrap(),
        Value::str("Gold")
    );
    // `required = true` from the model file holds in the database
    let err = tags
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The database refused the change: Missing required field 'Tag Name' (name) for model \
         'Tag' (test_data_only.tag)"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

fn x_foo_overlay() -> Overlay {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_field(
            "test_rel.unit",
            FieldDef::new("x_foo", KindDef::Char { size: None }).string("Foo"),
        )
        .unwrap();
    overlay
}

/// parent only, and children read and write it through `_inherits`. The `ir.model.fields`
/// records arrive with)
#[tokio::test]
async fn spec_model_045_custom_field_columns_on_parent_and_children() {
    let h = Harness::builder([&base_users::ADDON, &test_rel::ADDON])
        .overlay(x_foo_overlay())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    assert!(has(&columns(&mut tx, "test_rel_unit").await, "x_foo"));
    assert!(!has(&columns(&mut tx, "test_rel_box").await, "x_foo"));
    assert!(!has(&columns(&mut tx, "test_rel_pallet").await, "x_foo"));
    let pallet = tx
        .env()
        .records("test_rel.pallet")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("P1".into()))
                    .set_cell("x_foo", Cell::Str("through two levels".into())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        pallet.value(&mut tx, "x_foo").await.unwrap(),
        Value::str("through two levels")
    );
    tx.flush().await.unwrap();
    let stored = support::rows(&mut tx, "SELECT name, x_foo FROM test_rel_unit").await;
    assert_eq!(
        stored,
        vec![vec![
            Some("P1".to_owned()),
            Some("through two levels".to_owned())
        ]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// comes from `x_name`)
#[tokio::test]
async fn spec_model_044_custom_model_table_and_display_name() {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_model(ManualModel::new("x_bananas").description("Bananas"))
        .unwrap();
    overlay
        .add_manual_field(
            "x_bananas",
            FieldDef::new("x_name", KindDef::Char { size: None }).string("Name"),
        )
        .unwrap();
    overlay
        .add_manual_field("x_bananas", FieldDef::new("x_ripe", KindDef::Boolean))
        .unwrap();
    let h = Harness::builder([&base_users::ADDON])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let cols = columns(&mut tx, "x_bananas").await;
    for c in ["id", "x_name", "x_ripe", "create_uid", "write_date"] {
        assert!(has(&cols, c), "{c} missing from {cols:?}");
    }
    let banana = tx
        .env()
        .records("x_bananas")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("x_name", Cell::Str("Cavendish".into()))
                    .set_cell("x_ripe", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        banana.value(&mut tx, "display_name").await.unwrap(),
        Value::str("Cavendish")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// two databases with the same addons and different overlays — each sees only its
/// own custom field, in its registry and in its schema.
#[tokio::test]
async fn overlays_stay_in_their_database() {
    let custom = Harness::builder([&base_users::ADDON, &test_rel::ADDON])
        .overlay(x_foo_overlay())
        .build()
        .await
        .unwrap();
    let plain = Harness::builder([&base_users::ADDON, &test_rel::ADDON])
        .build()
        .await
        .unwrap();
    assert!(
        custom
            .registry()
            .model("test_rel.unit")
            .unwrap()
            .field("x_foo")
            .is_some()
    );
    assert!(
        plain
            .registry()
            .model("test_rel.unit")
            .unwrap()
            .field("x_foo")
            .is_none()
    );
    let mut tx = plain.begin().await.unwrap();
    assert!(!has(&columns(&mut tx, "test_rel_unit").await, "x_foo"));
    let err = tx
        .env()
        .records("test_rel.unit")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("U".into()))
                    .set_cell("x_foo", Cell::Str("no".into())),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "Invalid field 'x_foo' in 'test_rel.unit'");
    drop(tx);
    let mut tx = custom.begin().await.unwrap();
    assert!(has(&columns(&mut tx, "test_rel_unit").await, "x_foo"));
    drop(tx);
    custom.cleanup().await.unwrap();
    plain.cleanup().await.unwrap();
}

/// One unit row, so `test_rel_unit` is not empty.
async fn unit_row(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    tx.env()
        .records("test_rel.unit")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("U1".into()))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// The schema step of `Databases::set_overlay`: an ORM on the new registry syncs the schema.
fn syncing(
    h: &Harness,
) -> impl FnOnce(Arc<Registry>) -> BoxFuture<'static, erp_fields::Result<()>> + use<> {
    let pool = h.db().pool().clone();
    move |registry| {
        Box::pin(async move {
            let orm = Orm::builder(registry, pool)
                .policy(StaticPolicy::AllowAll)
                .build()?;
            let report = orm.sync_schema().await?;
            assert!(report.is_empty(), "{report:?}");
            Ok(())
        })
    }
}

fn required_code(default: Option<&str>) -> Overlay {
    let mut field = FieldDef::new("x_code", KindDef::Char { size: None }).required(true);
    if let Some(value) = default {
        field = field.default(DefaultValue::Str(value.to_owned()));
    }
    let mut overlay = Overlay::new();
    overlay.add_manual_field("test_rel.unit", field).unwrap();
    overlay
}

const REL_ADDONS: [&AddonDescriptor; 2] = [&base_users::ADDON, &test_rel::ADDON];

/// Review item 2: `Databases::set_overlay` syncs the schema with the new registry, then swaps; an
/// ORM on the returned registry reads and writes the new custom field.
#[tokio::test]
async fn overlay_then_schema_sync_then_crud() {
    let h = Harness::builder(REL_ADDONS).build().await.unwrap();
    let dbs = Databases::new(RegistryCache::new());
    let before = dbs.load("db", &REL_ADDONS, Overlay::new()).unwrap();
    let registry = dbs
        .set_overlay("db", x_foo_overlay(), syncing(&h))
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&dbs.registry("db").unwrap(), &registry));
    assert!(!Arc::ptr_eq(&registry, &before));
    let orm = Orm::builder(Arc::clone(&registry), h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert!(has(&columns(&mut tx, "test_rel_unit").await, "x_foo"));
    let unit = tx
        .env()
        .records("test_rel.unit")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("U".into()))
                    .set_cell("x_foo", Cell::Str("after the swap".into())),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let stored = support::rows(&mut tx, "SELECT x_foo FROM test_rel_unit").await;
    assert_eq!(stored, vec![vec![Some("after the swap".to_owned())]]);
    unit.set_value(&mut tx, "x_foo", Value::str("edited"))
        .await
        .unwrap();
    assert_eq!(
        unit.value(&mut tx, "x_foo").await.unwrap(),
        Value::str("edited")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review items 2 and 3: a required custom field without a default cannot be added to a table
/// with rows. The sync fails with the model, field and table named, rolls back, and the database
/// keeps its registry; with a default the same change goes through and fills the rows.
#[tokio::test]
async fn required_field_without_default_on_rows_is_refused() {
    let h = Harness::builder(REL_ADDONS).build().await.unwrap();
    unit_row(&h).await;
    let dbs = Databases::new(RegistryCache::new());
    let before = dbs.load("db", &REL_ADDONS, Overlay::new()).unwrap();
    let err = dbs
        .set_overlay("db", required_code(None), syncing(&h))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "cannot update the schema: required field 'test_rel.unit.x_code' has no default and \
         table \"test_rel_unit\" has rows; give the field a default, or add it as optional, fill \
         it, then make it required"
    );
    assert!(Arc::ptr_eq(&dbs.registry("db").unwrap(), &before));
    let mut tx = h.begin().await.unwrap();
    assert!(!has(&columns(&mut tx, "test_rel_unit").await, "x_code"));
    drop(tx);

    dbs.set_overlay("db", required_code(Some("N/A")), syncing(&h))
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let stored = support::rows(
        &mut tx,
        "SELECT x_code, (SELECT is_nullable::text FROM information_schema.columns \
         WHERE table_name = 'test_rel_unit' AND column_name = 'x_code') FROM test_rel_unit",
    )
    .await;
    assert_eq!(
        stored,
        vec![vec![Some("N/A".to_owned()), Some("NO".to_owned())]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 3: an existing optional column that becomes required without a default is refused
/// while it holds NULLs, and accepted once they are filled.
#[tokio::test]
async fn column_with_nulls_cannot_become_required_without_default() {
    let h = Harness::builder(REL_ADDONS).build().await.unwrap();
    unit_row(&h).await;
    let dbs = Databases::new(RegistryCache::new());
    dbs.load("db", &REL_ADDONS, Overlay::new()).unwrap();
    let mut optional = Overlay::new();
    optional
        .add_manual_field(
            "test_rel.unit",
            FieldDef::new("x_code", KindDef::Char { size: None }),
        )
        .unwrap();
    let optional_registry = dbs.set_overlay("db", optional, syncing(&h)).await.unwrap();
    let err = dbs
        .set_overlay("db", required_code(None), syncing(&h))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "cannot update the schema: required field 'test_rel.unit.x_code' has no default and \
         column \"test_rel_unit\".\"x_code\" holds NULL values; fill them or give the field a \
         default"
    );
    assert!(Arc::ptr_eq(
        &dbs.registry("db").unwrap(),
        &optional_registry
    ));
    let mut tx = h.begin().await.unwrap();
    tx.execute("UPDATE test_rel_unit SET x_code = 'filled'", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    dbs.set_overlay("db", required_code(None), syncing(&h))
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let nullable = support::rows(
        &mut tx,
        "SELECT is_nullable::text FROM information_schema.columns \
         WHERE table_name = 'test_rel_unit' AND column_name = 'x_code'",
    )
    .await;
    assert_eq!(nullable, vec![vec![Some("NO".to_owned())]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 3 and L1: the schema step bounds its own work with a lock timeout and a statement
/// timeout, and gives the caller's transaction its own settings back when it is done.
#[tokio::test]
async fn schema_transaction_is_bounded_and_restores_the_settings() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let settings = "SELECT current_setting('lock_timeout'), current_setting('statement_timeout')";
    support::rows(
        &mut tx,
        "SELECT set_config('lock_timeout', '7s', true), set_config('statement_timeout', '0', true)",
    )
    .await;
    let before = support::rows(&mut tx, settings).await;
    assert_eq!(
        before,
        vec![vec![Some("7s".to_owned()), Some("0".to_owned())]]
    );
    assert_ne!(SCHEMA_LOCK_TIMEOUT, "7s");
    assert_ne!(SCHEMA_STATEMENT_TIMEOUT, "0");
    tx.init_models(&[]).await.unwrap();
    assert_eq!(support::rows(&mut tx, settings).await, before);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review M1: a schema synchronization waiting for another one gives up after the schema lock
/// timeout with a clear error, instead of waiting (and holding a pool connection) for ever.
#[tokio::test]
async fn waiting_for_another_schema_sync_is_bounded() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut first = h.begin().await.unwrap();
    first.init_models(&[]).await.unwrap();
    // `first` holds the schema lock until it ends
    let mut second = h.begin().await.unwrap();
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(std::time::Duration::from_secs(120), second.init_models(&[]))
        .await
        .expect("the second schema sync must not wait for ever")
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(
        err.message(),
        format!(
            "another schema synchronization of this database is running; gave up waiting for it \
             after {SCHEMA_LOCK_TIMEOUT} (lock_timeout), try again when it has finished"
        )
    );
    assert_eq!(err.sqlstate(), Some("55P03"));
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
    drop(second);
    drop(first);
    h.cleanup().await.unwrap();
}

/// merge: the schema step only lowers the timeouts. A caller with a lower `lock_timeout`
/// (a module operation's) keeps it: waiting for another schema synchronization
/// gives up after the caller's value, and the error names that value.
#[tokio::test]
async fn schema_step_keeps_a_lower_caller_timeout() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut first = h.begin().await.unwrap();
    first.init_models(&[]).await.unwrap();
    // `first` holds the schema lock until it ends
    let mut second = h.begin().await.unwrap();
    support::rows(
        &mut second,
        "SELECT set_config('lock_timeout', '300ms', true), \
                set_config('statement_timeout', '20s', true)",
    )
    .await;
    let started = std::time::Instant::now();
    let err = second.init_models(&[]).await.unwrap_err();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "waited {:?}",
        started.elapsed()
    );
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(
        err.message(),
        "another schema synchronization of this database is running; gave up waiting for it \
         after 300ms (lock_timeout), try again when it has finished"
    );
    assert_eq!(err.sqlstate(), Some("55P03"));
    drop(second);
    drop(first);

    // a lower statement_timeout is kept during the step too, and both come back afterwards
    let mut tx = h.begin().await.unwrap();
    support::rows(
        &mut tx,
        "SELECT set_config('lock_timeout', '300ms', true), \
                set_config('statement_timeout', '20s', true)",
    )
    .await;
    tx.init_models(&[]).await.unwrap();
    assert_eq!(
        support::rows(
            &mut tx,
            "SELECT current_setting('lock_timeout'), current_setting('statement_timeout')"
        )
        .await,
        vec![vec![Some("300ms".to_owned()), Some("20s".to_owned())]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 8: a custom model's `_order` is parsed against its fields, never spliced into
/// SQL: a search with a hostile order fails with the search's own error, and the table stays.
#[tokio::test]
async fn model_order_is_never_spliced_into_sql() {
    let order = "x_name; DROP TABLE x_ord";
    let mut overlay = Overlay::new();
    overlay
        .add_manual_model(ManualModel::new("x_ord").order(order))
        .unwrap();
    overlay
        .add_manual_field(
            "x_ord",
            FieldDef::new("x_name", KindDef::Char { size: None }),
        )
        .unwrap();
    let h = Harness::builder([&base_users::ADDON])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .env()
        .records("x_ord")
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap_err();
    let expected = erp_sql::order::invalid_order_error(order);
    assert_eq!(
        (err.kind(), err.message()),
        (expected.kind(), expected.message())
    );
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert!(has(&columns(&mut tx, "x_ord").await, "x_name"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 8: relational `domain` and `context` texts of runtime fields are parsed into typed
/// values when the ORM starts (never spliced into SQL); text that is not a domain list or a
/// context object stops the start with the field named.
#[test]
fn field_domain_and_context_are_parsed_not_spliced() {
    let check = |field: FieldDef| {
        let mut overlay = Overlay::new();
        overlay.add_manual_field("test_rel.unit", field).unwrap();
        match Harness::builder(REL_ADDONS).overlay(overlay).check() {
            Err(HarnessError::Orm(err)) => err,
            other => panic!("expected an ORM error, got {other:?}"),
        }
    };
    let m2o = || {
        FieldDef::new(
            "x_owner_id",
            KindDef::Many2one {
                comodel: "res.users".into(),
                ondelete: None,
            },
        )
    };
    let domain = r#""1=1; DROP TABLE test_rel_unit""#;
    let err = check(m2o().domain(domain));
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("Field test_rel.unit.x_owner_id: invalid domain {domain:?}: expected a list")
    );
    let context = r#"["DROP TABLE test_rel_unit"]"#;
    let err = check(m2o().context(context));
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("Field test_rel.unit.x_owner_id: the context {context:?} is not an object")
    );
}
