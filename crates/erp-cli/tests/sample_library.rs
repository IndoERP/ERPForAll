//! The `sample_library` sample addon: a data-only addon (manifest, `models/*.toml`, XML views,
//! menus, an access CSV and demo data; no Rust crate) installed, used, upgraded and uninstalled
//! through the module system on a real database. The addon's folder is the one the human
//! release ships (`addons/sample_library`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use erp_fields::{Cell, Value};
use erp_modules::{
    AddonIndex, ModuleName,
    system::{InitOptions, ModuleSystem},
};
use erp_orm::rpc::{Exposure, RpcArgs};
use erp_orm::{Orm, RawVals, SUPERUSER_ID};
use erp_testkit::TestDb;

fn repo_addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        // Build output and installed JavaScript packages (symlinks) are not addon sources.
        if entry.file_name() == "node_modules" || entry.file_name() == "target" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn system_builder(db: &TestDb, addons: &Path) -> erp_modules::system::ModuleSystemBuilder {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons);
    // The catalog includes UoM and its native uninstall hook; data-only sample_library has none.
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
    ModuleSystem::builder(db.pool().clone(), index)
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(native_artifact_identities)
        .code([
            &erp_addon_uom::ADDON,
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_acceptance_notes::ADDON,
        ])
        .require_core_tables(erp_security::auth::core_tables())
}

fn system(db: &TestDb, addons: &Path) -> ModuleSystem {
    system_builder(db, addons).build().unwrap()
}

fn name() -> [ModuleName; 1] {
    [ModuleName::new("sample_library").unwrap()]
}

async fn scalar(orm: &Orm, sql: &str) -> String {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx.query(sql, &[]).await.unwrap();
    tx.rollback().await.unwrap();
    rows[0].get::<_, String>(0)
}

async fn columns(orm: &Orm, table: &str) -> String {
    scalar(
        orm,
        &format!(
            "SELECT coalesce(string_agg(column_name::text, ',' ORDER BY column_name), '') \
             FROM information_schema.columns WHERE table_schema = current_schema() \
             AND table_name = '{table}'"
        ),
    )
    .await
}

/// The actual installed base loan form, before and after an addon upgrade.
async fn assert_loan_statusbar(orm: &Orm, expected: bool) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let records = tx.env().records("sample_library.loan").unwrap();
    let Exposure::Exposed(method) = tx
        .exposure()
        .get(records.model_id(), "get_views")
        .unwrap()
        .clone()
    else {
        panic!("get_views is exposed")
    };
    let args = erp_fields::ValueMap::from_iter([
        (
            "views".into(),
            Value::List(vec![Value::List(vec![Value::False, Value::str("form")].into())].into()),
        ),
        ("wire_major".into(), Value::Int(1)),
        (
            "options".into(),
            Value::Map(Arc::new(erp_fields::ValueMap::from_iter([(
                "mobile".into(),
                Value::Bool(false),
            )]))),
        ),
    ]);
    let value = method
        .call(&mut tx, records, RpcArgs::new(args))
        .await
        .unwrap();
    let view = erp_fields::convert::value_to_json(&value).unwrap();
    let nodes = view["views"][0]["nodes"].as_array().unwrap();
    if !expected {
        assert!(!nodes.iter().any(|node| node["widget"] == "statusbar"));
        assert_eq!(
            nodes
                .iter()
                .filter(|node| node["field"]["name"] == "state")
                .count(),
            1
        );
        tx.rollback().await.unwrap();
        return;
    }
    assert_eq!(nodes[1]["kind"], "header");
    assert_eq!(nodes[1]["children"], serde_json::json!([2]));
    assert_eq!(nodes[2]["widget"], "statusbar");
    assert_eq!(nodes[2]["field"]["name"], "state");
    assert_eq!(
        nodes
            .iter()
            .filter(|node| node["field"]["name"] == "state")
            .count(),
        2
    );
    // The base addon keeps its pre-existing authorized editor; the header is display-only.
    assert!(
        nodes
            .iter()
            .any(|node| node["field"]["name"] == "state" && node["widget"] == "selection")
    );
    tx.rollback().await.unwrap();
}

/// default order; the table itself is the client's). Books by author then by year published (rows) and by format
/// (columns), measured in pages and price, with the count shown first.
async fn assert_book_pivot(orm: &Orm) {
    let view = book_view(orm, "pivot").await;
    assert_eq!(view["views"][0]["type"], "pivot");
    assert_eq!(view["views"][0]["renderer"], "web.pivot");
    let nodes = view["views"][0]["nodes"].as_array().unwrap();
    assert_eq!(nodes[0]["kind"], "pivot");
    assert_eq!(nodes[0]["disable_linking"], false);
    assert_eq!(nodes[0]["display_quantity"], true);
    assert_eq!(nodes[0]["default_order"], "pages desc");
    let roles: Vec<_> = nodes[1..]
        .iter()
        .map(|node| {
            (
                node["field"]["name"].as_str().unwrap(),
                node["role"].as_str().unwrap(),
                node.get("interval").and_then(|interval| interval.as_str()),
            )
        })
        .collect();
    assert_eq!(
        roles,
        [
            ("author_id", "row", None),
            ("published_on", "row", Some("year")),
            ("book_format", "col", None),
            ("pages", "measure", None),
            ("price", "measure", None)
        ]
    );
}

/// `get_views` of the book model for one view type, as the client receives it.
async fn book_view(orm: &Orm, view_type: &str) -> serde_json::Value {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let records = tx.env().records("sample_library.book").unwrap();
    let Exposure::Exposed(method) = tx
        .exposure()
        .get(records.model_id(), "get_views")
        .unwrap()
        .clone()
    else {
        panic!("get_views is exposed")
    };
    let args = erp_fields::ValueMap::from_iter([
        (
            "views".into(),
            Value::List(
                vec![Value::List(
                    vec![Value::False, Value::str(view_type)].into(),
                )]
                .into(),
            ),
        ),
        ("wire_major".into(), Value::Int(1)),
        (
            "options".into(),
            Value::Map(Arc::new(erp_fields::ValueMap::from_iter([(
                "mobile".into(),
                Value::Bool(false),
            )]))),
        ),
    ]);
    let value = method
        .call(&mut tx, records, RpcArgs::new(args))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    erp_fields::convert::value_to_json(&value).unwrap()
}

/// the books have a default search view, so the pivot's Group By menu has its items and the client's request for
/// a search view is answered (no refusal for a model without one).
async fn assert_book_search(orm: &Orm) {
    let view = book_view(orm, "search").await;
    assert_eq!(view["views"][0]["type"], "search");
    let nodes = view["views"][0]["nodes"].as_array().unwrap();
    let filters: Vec<_> = nodes
        .iter()
        .filter(|node| node["kind"] == "search_filter")
        .map(|node| node["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        filters,
        [
            "available",
            "popular",
            "group_author",
            "group_format",
            "group_published",
            "group_rating"
        ]
    );
}

/// Runs `body` on a thread with a large stack: a full install chain exceeds the default one.
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

/// The addon folder is data-only, as the sample promises.
#[test]
fn sample_library_has_no_rust_crate() {
    let dir = repo_addons().join("sample_library");
    for forbidden in ["Cargo.toml", "src", "build.rs"] {
        assert!(
            !dir.join(forbidden).exists(),
            "{forbidden} in a data-only addon"
        );
    }
    assert!(dir.join("manifest.toml").is_file());
    assert!(dir.join("models").is_dir());
}

/// Install, use through the ORM, upgrade with a schema change, and uninstall with cleanup.
#[test]
fn sample_library_install_use_upgrade_uninstall() {
    big_stack(install_use_upgrade_uninstall());
}

async fn install_use_upgrade_uninstall() {
    // A private copy of the addons folder, so the upgrade step can edit the sample.
    let work = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    copy_dir(&repo_addons(), work.path());
    // Rehearse the actual preceding 1.0.1 view shape, then upgrade to the shipped header.
    let views_path = work
        .path()
        .join("sample_library/views/sample_library_views.xml");
    let candidate_views = fs::read_to_string(&views_path).unwrap();
    // Exact historical view, rather than reconstructing it by substitutions in the new XML.
    let previous_views = include_str!("fixtures/sample_library_1_0_1_views.xml").to_owned();
    assert_ne!(previous_views, candidate_views);
    fs::write(&views_path, previous_views).unwrap();
    let manifest_path = work.path().join("sample_library/manifest.toml");
    let candidate_manifest = fs::read_to_string(&manifest_path).unwrap();
    fs::write(
        &manifest_path,
        candidate_manifest.replace("version = \"1.0.8\"", "version = \"1.0.1\""),
    )
    .unwrap();
    let db = TestDb::create().await.unwrap();
    let system = system(&db, work.path());
    let init = system.initialize(InitOptions::new()).await.unwrap();
    assert_eq!(
        scalar(
            &init.orm,
            "SELECT state FROM ir_module_module WHERE name = 'sample_library'"
        )
        .await,
        "uninstalled",
        "a data-only addon in the addons folder is listed but not installed"
    );

    // ---- install ----
    let env = system.system_env(erp_orm::Context::new());
    let installed = system
        .button_immediate_install(&env, &name())
        .await
        .unwrap();
    assert_eq!(installed.installed, name());
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    let orm = installed.orm;
    assert_loan_statusbar(&orm, false).await;
    for table in ["author", "genre", "book", "loan"] {
        assert!(
            !columns(&orm, &format!("sample_library_{table}"))
                .await
                .is_empty()
        );
    }
    // views, actions, menus and access rows came from the data files
    for (model, expected) in [
        ("ir.ui.view", 8),
        ("ir.actions.act_window", 4),
        ("ir.ui.menu", 5),
        ("ir.access", 4),
    ] {
        assert_eq!(
            scalar(
                &orm,
                &format!(
                    "SELECT count(*)::text FROM ir_model_data WHERE module = 'sample_library' \
                     AND model = '{model}'"
                )
            )
            .await,
            expected.to_string(),
            "{model}"
        );
    }
    assert_eq!(
        scalar(
            &orm,
            "SELECT count(*)::text FROM ir_model_data WHERE module = 'sample_library' \
             AND name = 'author_demo_le_guin'"
        )
        .await,
        "0",
        "demo data is not loaded without demo"
    );

    // ---- records through the ORM ----
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let author = tx
        .env()
        .records("sample_library.author")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Octavia Butler".into()))],
        )
        .await
        .unwrap();
    let book = tx
        .env()
        .records("sample_library.book")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("title", Cell::Str("Kindred".into()))
                    .set_cell("author_id", Cell::Id(author.ids()[0]))
                    .set_cell("publication_year", Cell::Int(1979)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        book.value(&mut tx, "available").await.unwrap(),
        Value::Bool(true)
    );
    let partner = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Reader".into()))],
        )
        .await
        .unwrap();
    let loan = tx
        .env()
        .records("sample_library.loan")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("book_id", Cell::Id(book.ids()[0]))
                    .set_cell("borrower_id", Cell::Id(partner.ids()[0])),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        loan.value(&mut tx, "state").await.unwrap(),
        Value::str("draft")
    );
    tx.commit().await.unwrap();
    // a required field is enforced
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let missing_title = tx
        .env()
        .records("sample_library.book")
        .unwrap()
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap_err();
    assert_eq!(missing_title.kind(), erp_fields::ErrorKind::Validation);
    drop(tx);

    // ---- upgrade with a schema change ----
    // Model an explicit forward release: preserve the installed preimage before editing
    // the candidate, then publish its receipt after the successful migration below.
    let previous: std::collections::BTreeMap<ModuleName, String> = system.with_index(|index| {
        index
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
            .collect()
    });
    fs::write(&views_path, candidate_views).unwrap();
    fs::write(&manifest_path, candidate_manifest).unwrap();
    let models = work.path().join("sample_library/models/20_book.toml");
    let mut text = fs::read_to_string(&models).unwrap();
    text.push_str("\n[[model.field]]\nname = \"shelf\"\ntype = \"char\"\nstring = \"Shelf\"\n");
    fs::write(&models, text).unwrap();
    let manifest = work.path().join("sample_library/manifest.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    fs::write(
        &manifest,
        text.replace("version = \"1.0.8\"", "version = \"1.1.0\""),
    )
    .unwrap();
    let candidate = system_builder(&db, work.path())
        .identity_previous(previous.clone())
        .build()
        .unwrap();
    assert!(!columns(&orm, "sample_library_book").await.contains("shelf"));
    let candidate_env = candidate.system_env(erp_orm::Context::new());
    let upgraded = candidate
        .button_immediate_upgrade(&candidate_env, &name())
        .await
        .unwrap();
    assert_eq!(upgraded.upgraded, name());
    assert!(
        upgraded.post_commit.is_clean(),
        "{:?}",
        upgraded.post_commit
    );
    let orm = upgraded.orm;
    assert_loan_statusbar(&orm, true).await;
    assert_book_pivot(&orm).await;
    assert_book_search(&orm).await;
    assert!(columns(&orm, "sample_library_book").await.contains("shelf"));
    assert_eq!(
        scalar(&orm, "SELECT count(*)::text FROM sample_library_book").await,
        "1",
        "the upgrade keeps the data"
    );

    candidate
        .identity_activate(
            &previous,
            "fixture-operator",
            "verified forward fixture release",
        )
        .await
        .unwrap();
    let system = self::system(&db, work.path());
    let env = system.system_env(erp_orm::Context::new());
    // ---- uninstall ----
    let removed = system
        .button_immediate_uninstall(&env, &name())
        .await
        .unwrap();
    assert_eq!(removed.uninstalled, name());
    let orm = removed.orm;
    assert_eq!(
        scalar(
            &orm,
            "SELECT state FROM ir_module_module WHERE name = 'sample_library'"
        )
        .await,
        "uninstalled"
    );
    for table in ["author", "genre", "book", "loan"] {
        assert!(
            columns(&orm, &format!("sample_library_{table}"))
                .await
                .is_empty()
        );
    }
    assert_eq!(
        scalar(
            &orm,
            "SELECT count(*)::text FROM ir_model_data WHERE module = 'sample_library'"
        )
        .await,
        "0"
    );
    assert_eq!(
        scalar(
            &orm,
            "SELECT count(*)::text FROM ir_ui_view WHERE model LIKE 'sample_library.%'"
        )
        .await,
        "0"
    );
    assert_eq!(
        scalar(
            &orm,
            "SELECT count(*)::text FROM ir_ui_menu WHERE name = 'Library'"
        )
        .await,
        "0"
    );
}

/// Demo rows load when the database is created with demo data.
#[test]
fn sample_library_demo_data_loads_with_demo() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db, &repo_addons());
        let _initialized = system
            .initialize(InitOptions::new().with_demo(true))
            .await
            .unwrap();
        let env = system.system_env(erp_orm::Context::new());
        let installed = system
            .button_immediate_install(&env, &name())
            .await
            .unwrap();
        assert!(installed.post_commit.is_clean());
        let orm = installed.orm;
        assert_eq!(
            scalar(&orm, "SELECT count(*)::text FROM sample_library_author").await,
            "6"
        );
        assert_eq!(
            scalar(&orm, "SELECT count(*)::text FROM sample_library_book").await,
            "12"
        );
        assert_eq!(
            scalar(&orm, "SELECT count(*)::text FROM sample_library_genre").await,
            "6"
        );
        assert_eq!(
            scalar(
                &orm,
                "SELECT a.name FROM sample_library_book b JOIN sample_library_author a \
                 ON a.id = b.author_id WHERE b.title = 'The Dispossessed'"
            )
            .await,
            "Ursula K. Le Guin"
        );
        assert_eq!(
            scalar(
                &orm,
                "SELECT b.description FROM sample_library_book b WHERE b.title = 'The Dispossessed'"
            )
            .await,
            "<p>A physicist travels between two worlds with opposite social orders, exploring <strong>anarchism</strong> and capitalism.</p>"
        );
        // the books' genres (many2many) and loans (one2many through loan.book_id)
        assert_eq!(
            scalar(
                &orm,
                "SELECT count(*)::text FROM sample_library_book_sample_library_genre_rel r \
                 JOIN sample_library_book b ON b.id = r.sample_library_book_id \
                 WHERE b.title = 'A Wizard of Earthsea'"
            )
            .await,
            "2"
        );
        assert_eq!(
            scalar(
                &orm,
                "SELECT string_agg(l.state::text, ',' ORDER BY l.state) FROM sample_library_loan l \
                 JOIN sample_library_book b ON b.id = l.book_id WHERE b.title = 'Dune'"
            )
            .await,
            "out,returned"
        );
    });
}

/// A broken model file stops the install with the file and line, and nothing persists.
#[test]
fn sample_library_broken_model_file_names_the_file() {
    big_stack(async {
        let work = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        copy_dir(&repo_addons(), work.path());
        fs::write(
            work.path().join("sample_library/models/10_author.toml"),
            "[[model]]\nname = \"sample_library.author\"\n[[model.field]]\nname = \"name\"\ntype = \"nonsense\"\n",
        )
        .unwrap();
        let db = TestDb::create().await.unwrap();
        let system = system(&db, work.path());
        let initialized = system.initialize(InitOptions::new()).await.unwrap();
        let env = system.system_env(erp_orm::Context::new());
        let error = system
            .button_immediate_install(&env, &name())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("10_author.toml"), "{error}");
        assert!(error.contains("nonsense"), "{error}");
        assert_eq!(
            scalar(
                &initialized.orm,
                "SELECT state FROM ir_module_module WHERE name = 'sample_library'"
            )
            .await,
            "uninstalled"
        );
    });
}

/// An addon copied into the addons folder after the database was created installs without a
/// separate list refresh: the install operation registers new addons first.
#[test]
fn sample_library_added_after_database_creation_installs() {
    big_stack(async {
        let work = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        for entry in ["base", "web", "acceptance_notes"] {
            copy_dir(&repo_addons().join(entry), &work.path().join(entry));
        }
        let db = TestDb::create().await.unwrap();
        let system = system(&db, work.path());
        let initialized = system.initialize(InitOptions::new()).await.unwrap();
        assert_eq!(
            scalar(
                &initialized.orm,
                "SELECT count(*)::text FROM ir_module_module WHERE name = 'sample_library'"
            )
            .await,
            "0"
        );
        copy_dir(
            &repo_addons().join("sample_library"),
            &work.path().join("sample_library"),
        );
        system.with_index(AddonIndex::invalidate_cache);
        let env = system.system_env(erp_orm::Context::new());
        let installed = system
            .button_immediate_install(&env, &name())
            .await
            .unwrap();
        assert_eq!(installed.installed, name());
    });
}
