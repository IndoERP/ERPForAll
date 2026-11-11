//! Two-addon extension integration suite.
//! Tests full lifecycle A -> B -> upgrade A -> uninstall B -> uninstall A,
//! and data-only module lifecycle .
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use erp_addon_test_addon_a::{Book, BookMethods};
use erp_addon_test_addon_b::BookExtension;
use erp_modules::discovery::AddonIndex;
use erp_modules::manifest::ModuleName;
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_orm::{Context, Env, SUPERUSER_ID, Transaction};
use erp_sdk::{Domain, Operator, Vals};
use erp_testkit::TestDb;
use tempfile::TempDir;

fn n(name: &str) -> ModuleName {
    ModuleName::new(name).unwrap()
}

fn su(orm: &erp_orm::Orm) -> Env {
    orm.env(SUPERUSER_ID, Context::new())
}

async fn columns(tx: &mut Transaction, table: &str) -> Vec<String> {
    support::rows(
        tx,
        &format!(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = '{table}' ORDER BY ordinal_position"
        ),
    )
    .await
    .into_iter()
    .map(|row| row[0].clone().unwrap())
    .collect()
}

async fn module_state(tx: &mut Transaction, name: &str) -> String {
    support::rows(
        tx,
        &format!("SELECT state::text FROM ir_module_module WHERE name = '{name}'"),
    )
    .await
    .remove(0)
    .remove(0)
    .unwrap()
}

fn has(columns: &[String], name: &str) -> bool {
    columns.iter().any(|c| c == name)
}

async fn table_exists(tx: &mut Transaction, table: &str) -> bool {
    let res = tx
        .query("SELECT (to_regclass($1) IS NOT NULL)::bool", &[&table])
        .await
        .unwrap();
    res[0].get::<_, bool>(0)
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let dest_child = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_all(&path, &dest_child);
        } else {
            fs::copy(&path, &dest_child).unwrap();
        }
    }
}

fn setup_dev_folder() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let dev_path = tmp.path().to_path_buf();

    // 1. minimal base manifest
    let base_dir = dev_path.join("base");
    fs::create_dir_all(&base_dir).unwrap();
    fs::write(
        base_dir.join("manifest.toml"),
        "name = 'Base'\nsummary = 'Kernel'\nversion = '1.0'\nauto_install = true\ncategory = 'Hidden'\n",
    )
    .unwrap();

    // 2. copy test_addon_a, test_addon_b, test_data_only from crates/erp-test-models/addons
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("addons");
    for addon in ["test_addon_a", "test_addon_b", "test_data_only"] {
        let src = src_root.join(addon);
        let dst = dev_path.join(addon);
        copy_dir_all(&src, &dst);
    }

    (tmp, dev_path)
}

#[test]
fn spec_mod_two_addon_extension_suite_full_lifecycle() {
    // This test overflowed its thread stack on Linux CI and under a constrained
    // test-thread stack on macOS. Construct and run it on a dedicated stack;
    // keep the current-thread Tokio runtime and every lifecycle assertion below.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(full_lifecycle());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn full_lifecycle() {
    let db = TestDb::builder().pool_size(8).create().await.unwrap();
    let (_tmp, dev_path) = setup_dev_folder();

    let mut index = AddonIndex::new(true);
    index.add_dev_path(&dev_path);

    // This binary links code for these private fixture artifacts. Bind its provenance to
    // the explicit physical fixture bytes rather than exempting tests from admission.
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
        .native_artifact_identities(native_artifact_identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_test_addon_a::ADDON,
            &erp_addon_test_addon_b::ADDON,
        ])
        .build()
        .unwrap();

    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();

    // -------------------------------------------------------------------------
    // Phase 1: Install A
    // -------------------------------------------------------------------------
    let outcome_a = system
        .button_immediate_install(&su(&init.orm), &[n("test_addon_a")])
        .await
        .unwrap();

    {
        let mut tx = outcome_a.orm.begin(SUPERUSER_ID).await.unwrap();
        assert!(table_exists(&mut tx, "test_addon_book").await);
        let cols_a = columns(&mut tx, "test_addon_book").await;
        assert!(has(&cols_a, "name"));
        assert!(has(&cols_a, "author"));
        assert!(has(&cols_a, "published_year"));
        assert!(!has(&cols_a, "isbn"));
        assert!(!has(&cols_a, "rating"));

        // Verify records loaded from data/books.xml
        let dune_rows = support::rows(
            &mut tx,
            "SELECT name, author, published_year::text FROM test_addon_book WHERE name = 'Dune'",
        )
        .await;
        assert_eq!(
            dune_rows,
            vec![vec![
                Some("Dune".to_owned()),
                Some("Frank Herbert".to_owned()),
                Some("1965".to_owned()),
            ]]
        );

        // Verify external IDs in ir_model_data
        let xids_a = support::rows(
            &mut tx,
            "SELECT name, model FROM ir_model_data WHERE module = 'test_addon_a' ORDER BY name",
        )
        .await;
        assert!(xids_a.iter().any(|r| r[0].as_deref() == Some("book_dune")));
        assert!(
            xids_a
                .iter()
                .any(|r| r[0].as_deref() == Some("book_foundation"))
        );
        assert!(
            xids_a
                .iter()
                .any(|r| r[0].as_deref() == Some("view_book_form"))
        );
        assert!(
            xids_a
                .iter()
                .any(|r| r[0].as_deref() == Some("view_book_tree"))
        );

        // Verify views registered in ir_ui_view
        let views_a = support::rows(
            &mut tx,
            "SELECT name, mode FROM ir_ui_view WHERE model = 'test_addon.book' ORDER BY name",
        )
        .await;
        assert!(
            views_a
                .iter()
                .any(|r| r[0].as_deref() == Some("test_addon.book.form"))
        );
        assert!(
            views_a
                .iter()
                .any(|r| r[0].as_deref() == Some("test_addon.book.tree"))
        );

        // Verify Book model method describe()
        let dune = tx
            .env()
            .model::<Book>()
            .unwrap()
            .search(
                &mut tx,
                &Domain::condition("name", Operator::Eq, "Dune").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            dune.describe(&mut tx).await.unwrap(),
            "Dune by Frank Herbert"
        );

        tx.commit().await.unwrap();
    }

    // -------------------------------------------------------------------------
    // Phase 2: Install B
    // -------------------------------------------------------------------------
    let outcome_b = system
        .button_immediate_install(&su(&outcome_a.orm), &[n("test_addon_b")])
        .await
        .unwrap();

    {
        let mut tx = outcome_b.orm.begin(SUPERUSER_ID).await.unwrap();
        let cols_b = columns(&mut tx, "test_addon_book").await;
        assert!(has(&cols_b, "isbn"));
        assert!(has(&cols_b, "rating"));

        // Verify inherited view in ir_ui_view
        let inherit_view = support::rows(
            &mut tx,
            "SELECT name, mode, (inherit_id IS NOT NULL)::text FROM ir_ui_view WHERE name = 'test_addon.book.form.inherit'",
        )
        .await;
        assert_eq!(
            inherit_view,
            vec![vec![
                Some("test_addon.book.form.inherit".to_owned()),
                Some("extension".to_owned()),
                Some("true".to_owned()),
            ]]
        );

        // Create record using extended fields
        let hyperion = tx
            .env()
            .model::<Book>()
            .unwrap()
            .create(
                &mut tx,
                vec![
                    Vals::new()
                        .set(Book::NAME, "Hyperion")
                        .set(Book::AUTHOR, "Dan Simmons")
                        .set(Book::PUBLISHED_YEAR, 1989)
                        .set(BookExtension::ISBN, "978-0553283686")
                        .set(BookExtension::RATING, 4.9),
                ],
            )
            .await
            .unwrap();

        // Method describe() on hyperion runs B's override calling super
        assert_eq!(
            hyperion.describe(&mut tx).await.unwrap(),
            "Hyperion by Dan Simmons (Rating: 4.9)"
        );

        // Update Dune with rating and verify describe()
        let dune = tx
            .env()
            .model::<Book>()
            .unwrap()
            .search(
                &mut tx,
                &Domain::condition("name", Operator::Eq, "Dune").unwrap(),
            )
            .await
            .unwrap();
        dune.write(&mut tx, Vals::new().set(BookExtension::RATING, 4.8))
            .await
            .unwrap();
        assert_eq!(
            dune.describe(&mut tx).await.unwrap(),
            "Dune by Frank Herbert (Rating: 4.8)"
        );

        tx.commit().await.unwrap();
    }

    // -------------------------------------------------------------------------
    // Phase 3: Upgrade A
    // -------------------------------------------------------------------------
    let outcome_up = system
        .button_immediate_upgrade(&su(&outcome_b.orm), &[n("test_addon_a")])
        .await
        .unwrap();
    assert!(outcome_up.post_commit.failed.is_empty());

    {
        let mut tx = outcome_up.orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(module_state(&mut tx, "test_addon_a").await, "installed");
        assert_eq!(module_state(&mut tx, "test_addon_b").await, "installed");
        let dune = tx
            .env()
            .model::<Book>()
            .unwrap()
            .search(
                &mut tx,
                &Domain::condition("name", Operator::Eq, "Dune").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(dune.get(&mut tx, BookExtension::RATING).await.unwrap(), 4.8);
        assert_eq!(
            dune.describe(&mut tx).await.unwrap(),
            "Dune by Frank Herbert (Rating: 4.8)"
        );
        tx.commit().await.unwrap();
    }

    // -------------------------------------------------------------------------
    // Phase 4: Uninstall B
    // -------------------------------------------------------------------------
    let outcome_un_b = system
        .button_immediate_uninstall(&su(&outcome_up.orm), &[n("test_addon_b")])
        .await
        .unwrap();

    {
        let mut tx = outcome_un_b.orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(module_state(&mut tx, "test_addon_b").await, "uninstalled");
        assert_eq!(module_state(&mut tx, "test_addon_a").await, "installed");
        let b_xids = support::rows(
            &mut tx,
            "SELECT count(*)::text FROM ir_model_data WHERE module = 'test_addon_b'",
        )
        .await;
        assert_eq!(b_xids[0][0].as_deref(), Some("0"));
        let cols = columns(&mut tx, "test_addon_book").await;
        assert!(!has(&cols, "isbn") && !has(&cols, "rating"));
        assert!(has(&cols, "published_year"));
        // B's views are purged from ir_ui_view
        let b_views = support::rows(
            &mut tx,
            "SELECT name FROM ir_ui_view WHERE name = 'test_addon.book.form.inherit'",
        )
        .await;
        assert!(b_views.is_empty());

        // Method describe() reverts to A's base implementation
        let dune = tx
            .env()
            .model::<Book>()
            .unwrap()
            .search(
                &mut tx,
                &Domain::condition("name", Operator::Eq, "Dune").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            dune.describe(&mut tx).await.unwrap(),
            "Dune by Frank Herbert"
        );
        tx.commit().await.unwrap();
    }

    // -------------------------------------------------------------------------
    // Phase 5: Uninstall A
    // -------------------------------------------------------------------------
    let outcome_un_a = system
        .button_immediate_uninstall(&su(&outcome_un_b.orm), &[n("test_addon_a")])
        .await
        .unwrap();

    {
        let mut tx = outcome_un_a.orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(module_state(&mut tx, "test_addon_a").await, "uninstalled");
        assert!(!table_exists(&mut tx, "test_addon_book").await);
        let a_xids = support::rows(
            &mut tx,
            "SELECT count(*)::text FROM ir_model_data WHERE module = 'test_addon_a'",
        )
        .await;
        assert_eq!(a_xids[0][0].as_deref(), Some("0"));
        tx.commit().await.unwrap();
    }
}

#[tokio::test]
async fn spec_mod_045_data_only_module_lifecycle() {
    let db = TestDb::builder().pool_size(8).create().await.unwrap();
    let (_tmp, dev_path) = setup_dev_folder();

    // The module system reads `test_data_only/models/*.toml` from the dev folder itself
    // (data-only addon); passing the same declarations again would define the
    // model twice.
    let mut index = AddonIndex::new(true);
    index.add_dev_path(&dev_path);

    // This binary links code for these private fixture artifacts. Bind its provenance to
    // the explicit physical fixture bytes rather than exempting tests from admission.
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
        .native_artifact_identities(native_artifact_identities)
        .code([&erp_addon_base::ADDON])
        .build()
        .unwrap();

    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();

    // Install test_data_only
    let outcome_data = system
        .button_immediate_install(&su(&init.orm), &[n("test_data_only")])
        .await
        .unwrap();

    {
        let mut tx = outcome_data.orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(module_state(&mut tx, "test_data_only").await, "installed");
        assert!(table_exists(&mut tx, "test_data_only_tag").await);
        let tag_cols = columns(&mut tx, "test_data_only_tag").await;
        assert!(has(&tag_cols, "name"));
        assert!(has(&tag_cols, "sequence"));
        assert!(has(&tag_cols, "color"));

        // Data loaded from data/tags.xml
        let tags = support::rows(
            &mut tx,
            "SELECT name, sequence::text, color FROM test_data_only_tag ORDER BY sequence",
        )
        .await;
        assert_eq!(
            tags,
            vec![
                vec![
                    Some("Fiction".to_owned()),
                    Some("1".to_owned()),
                    Some("red".to_owned()),
                ],
                vec![
                    Some("Science".to_owned()),
                    Some("2".to_owned()),
                    Some("green".to_owned()),
                ],
            ]
        );

        // External IDs exist
        let tag_xids = support::rows(
            &mut tx,
            "SELECT name, model FROM ir_model_data WHERE module = 'test_data_only' AND model = 'test_data_only.tag' ORDER BY name",
        )
        .await;
        assert_eq!(
            tag_xids,
            vec![
                vec![
                    Some("tag_fiction".to_owned()),
                    Some("test_data_only.tag".to_owned()),
                ],
                vec![
                    Some("tag_science".to_owned()),
                    Some("test_data_only.tag".to_owned()),
                ],
            ]
        );

        let model_xid = support::rows(
            &mut tx,
            "SELECT count(*)::text FROM ir_model_data WHERE module = 'test_data_only' AND model = 'ir.model'",
        )
        .await;
        assert_eq!(model_xid[0][0].as_deref(), Some("1"));

        tx.commit().await.unwrap();
    }

    // Uninstall test_data_only
    let outcome_un_data = system
        .button_immediate_uninstall(&su(&outcome_data.orm), &[n("test_data_only")])
        .await
        .unwrap();

    {
        let mut tx = outcome_un_data.orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(module_state(&mut tx, "test_data_only").await, "uninstalled");
        assert!(!table_exists(&mut tx, "test_data_only_tag").await);
        let tag_xids_post = support::rows(
            &mut tx,
            "SELECT count(*)::text FROM ir_model_data WHERE module = 'test_data_only'",
        )
        .await;
        assert_eq!(tag_xids_post[0][0].as_deref(), Some("0"));
        tx.commit().await.unwrap();
    }
}
