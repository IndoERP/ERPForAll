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

#[path = "support/report_frame.rs"]
mod report_frame;

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

/// loan date to the due date, coloured by borrower, with borrower and status filters)
async fn assert_loan_calendar(orm: &Orm) {
    let view = model_view(orm, "sample_library.loan", "calendar").await;
    assert_eq!(view["views"][0]["type"], "calendar");
    assert_eq!(view["views"][0]["renderer"], "web.calendar");
    let nodes = view["views"][0]["nodes"].as_array().unwrap();
    assert_eq!(nodes[0]["kind"], "calendar");
    assert_eq!(nodes[0]["date_start"]["name"], "loan_date");
    assert_eq!(nodes[0]["date_stop"]["name"], "due_date");
    assert_eq!(nodes[0]["color"]["name"], "borrower_id");
    assert_eq!(nodes[0]["mode"], "month");
    let fields: Vec<_> = nodes[1..]
        .iter()
        .map(|node| {
            (
                node["field"]["name"].as_str().unwrap(),
                node["filters"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        fields,
        [
            ("book_id", false),
            ("borrower_id", true),
            ("state", true),
            ("return_date", false)
        ]
    );
}

async fn assert_loan_kanban(orm: &Orm) {
    let view = model_view(orm, "sample_library.loan", "kanban").await;
    assert_eq!(view["views"][0]["renderer"], "web.kanban");
    let nodes = view["views"][0]["nodes"].as_array().unwrap();
    assert_eq!(nodes[0]["kind"], "kanban");
    assert_eq!(nodes[0]["default_group_by"]["name"], "state");
    let draft = |answer: &serde_json::Value| {
        let groups = answer["groups"].as_array().unwrap();
        assert_eq!(answer["length"], groups.len());
        groups
            .iter()
            .find(|group| group["state"] == "draft")
            .unwrap()
            .clone()
    };
    let column = draft(&loan_board(orm, None).await);
    let count = column["__count"].as_i64().unwrap();
    assert!(count >= 1, "the loan created above is a draft");
    let cards = column["__records"].as_array().unwrap();
    assert_eq!(cards.len() as i64, count.min(40));
    assert!(cards[0]["id"].as_i64().unwrap() > 0);
    assert!(cards[0].get("book_id").is_some());
    // The client admits exactly these members on a column (`decodeKanbanGroups` in kanban-columns.ts).
    let mut members: Vec<_> = column.as_object().unwrap().keys().cloned().collect();
    members.sort_unstable();
    assert_eq!(members, ["__count", "__extra_domain", "__records", "state"]);
    let folded = draft(
        &loan_board(
            orm,
            Some(serde_json::json!([{"value": "draft", "folded": true}])),
        )
        .await,
    );
    assert_eq!(folded["__count"], count);
    assert!(folded.get("__records").is_none());
    let opened = draft(
        &loan_board(
            orm,
            Some(
                serde_json::json!([{"value": "draft", "folded": false, "limit": 40, "offset": 0}]),
            ),
        )
        .await,
    );
    assert_eq!(opened["__records"].as_array().unwrap().len(), cards.len());
}

/// The grouped read of the Loans board with the arguments the web client sends (`loadBoard` in
/// addons/web/static/src/client/app.ts): every status, its count, and the first cards of each open column.
async fn loan_board(orm: &Orm, opening_info: Option<serde_json::Value>) -> serde_json::Value {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let records = tx.env().records("sample_library.loan").unwrap();
    let Exposure::Exposed(method) = tx
        .exposure()
        .get(records.model_id(), "web_read_group")
        .unwrap()
        .clone()
    else {
        panic!("web_read_group is exposed")
    };
    let mut args = serde_json::json!({
        "domain": [],
        "groupby": ["state"],
        "aggregates": ["__count"],
        "limit": 100,
        "auto_unfold": true,
        "unfold_read_specification": {
            "book_id": {"fields": {"display_name": {}}},
            "borrower_id": {"fields": {"display_name": {}}},
            "loan_date": {},
            "due_date": {}
        },
        "unfold_read_default_limit": 40
    });
    if let Some(info) = opening_info {
        args["opening_info"] = info;
    }
    let Value::Map(args) = rpc_value(&args) else {
        panic!("the arguments are an object")
    };
    let value = method
        .call(&mut tx, records, RpcArgs::new((*args).clone()))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    erp_fields::convert::value_to_json(&value).unwrap()
}

/// JSON arguments as the RPC values a request carries (booleans, integers, text, lists and objects only).
fn rpc_value(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Bool(flag) => Value::Bool(*flag),
        serde_json::Value::Number(number) => Value::Int(number.as_i64().unwrap()),
        serde_json::Value::String(text) => Value::str(text.as_str()),
        serde_json::Value::Array(items) => {
            Value::List(items.iter().map(rpc_value).collect::<Vec<_>>().into())
        }
        serde_json::Value::Object(members) => {
            Value::Map(Arc::new(erp_fields::ValueMap::from_iter(
                members
                    .iter()
                    .map(|(name, item)| (name.as_str().into(), rpc_value(item))),
            )))
        }
        serde_json::Value::Null => panic!("these arguments hold no null"),
    }
}

/// regions, field widgets and progress bar; the cards themselves are the client's)
async fn assert_book_kanban(orm: &Orm) {
    let view = book_view(orm, "kanban").await;
    assert_eq!(view["views"][0]["type"], "kanban");
    assert_eq!(view["views"][0]["renderer"], "web.kanban");
    let nodes = view["views"][0]["nodes"].as_array().unwrap();
    let kinds: Vec<_> = nodes
        .iter()
        .map(|node| {
            let kind = node["kind"].as_str().unwrap();
            match kind {
                "field" => format!(
                    "{}:{}",
                    node["field"]["name"].as_str().unwrap(),
                    node["widget"].as_str().unwrap()
                ),
                "kanban_section" => node["region"].as_str().unwrap().to_owned(),
                _ => kind.to_owned(),
            }
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "kanban",
            "kanban_progressbar",
            "kanban_card",
            "main",
            "title:char",
            "author_id:many2one",
            "website:url",
            "aside",
            "title:char",
            "footer",
            "genre_ids:many2many_tags",
            "rating:priority",
            "favorite:boolean_favorite",
        ]
    );
    assert_eq!(nodes[0]["quick_create"], true);
    assert_eq!(nodes[1]["field"]["name"], "book_format");
    assert_eq!(
        nodes[1]["colors"],
        serde_json::json!([
            {"value": "hardcover", "tone": "success"},
            {"value": "paperback", "tone": "info"},
            {"value": "ebook", "tone": "warning"}
        ])
    );
}

/// flags and fields in order; the chart itself is the client's). Books per year published, one series per format.
async fn assert_book_graph(orm: &Orm) {
    let view = book_view(orm, "graph").await;
    assert_eq!(view["views"][0]["type"], "graph");
    assert_eq!(view["views"][0]["renderer"], "web.graph");
    let nodes = view["views"][0]["nodes"].as_array().unwrap();
    assert_eq!(nodes[0]["kind"], "graph");
    assert_eq!(nodes[0]["mode"], "bar");
    assert_eq!(nodes[0]["stacked"], true);
    assert_eq!(nodes[0]["cumulated"], false);
    assert_eq!(nodes[0]["disable_linking"], false);
    assert!(nodes[0].get("order").is_none());
    let fields: Vec<_> = nodes[1..]
        .iter()
        .map(|node| {
            (
                node["field"]["name"].as_str().unwrap(),
                node["role"].as_str().unwrap(),
                node.get("interval").and_then(|interval| interval.as_str()),
                node["label"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        fields,
        [
            ("published_on", "group", Some("year"), "Published On"),
            ("book_format", "group", None, "Format"),
            ("price", "measure", None, "Price"),
            ("pages", "measure", None, "Pages")
        ]
    );
}

/// `get_views` of the book model for one view type, as the client receives it.
async fn book_view(orm: &Orm, view_type: &str) -> serde_json::Value {
    model_view(orm, "sample_library.book", view_type).await
}

/// `get_views` of one model for one view type, as the client receives it.
async fn model_view(orm: &Orm, model: &str, view_type: &str) -> serde_json::Value {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let records = tx.env().records(model).unwrap();
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
        candidate_manifest.replace("version = \"1.0.14\"", "version = \"1.0.1\""),
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
        // eight views plus the six report templates
        ("ir.ui.view", 14),
        ("ir.actions.act_window", 4),
        ("ir.actions.report", 4),
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
        text.replace("version = \"1.0.14\"", "version = \"1.1.0\""),
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
    assert_loan_calendar(&orm).await;
    assert_book_kanban(&orm).await;
    assert_loan_kanban(&orm).await;
    assert_book_graph(&orm).await;
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
        // a loan is named by its book (`rec_name = "book_id"`), never "sample_library.loan,<id>"
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let rows = tx
            .query(
                "SELECT l.id::bigint, b.title::text FROM sample_library_loan l \
                 JOIN sample_library_book b ON b.id = l.book_id ORDER BY l.id",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 6, "the demo loans");
        for row in rows {
            let (id, title) = (row.get::<_, i64>(0), row.get::<_, String>(1));
            let loan = tx
                .env()
                .records("sample_library.loan")
                .unwrap()
                .browse(vec![id])
                .unwrap();
            assert_eq!(
                loan.cell(&mut tx, "display_name").await.unwrap(),
                Cell::Str(Arc::from(title.as_str())),
                "loan {id}"
            );
        }
        tx.rollback().await.unwrap();
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

#[test]
fn sample_library_dashboard_reads_installed_declaration_and_scoped_records() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db, &repo_addons());
        let initialized = system.initialize(InitOptions::new()).await.unwrap();
        drop(initialized);
        let env = system.system_env(erp_orm::Context::new());
        let orm = system
            .button_immediate_install(&env, &name())
            .await
            .unwrap()
            .orm;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let books = tx.env().records("sample_library.book").unwrap();
        books
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("title", Cell::Str("Dashboard A".into()))
                        .set_cell("available", Cell::Bool(true))
                        .set_cell("pages", Cell::Int(12)),
                    RawVals::new()
                        .set_cell("title", Cell::Str("Dashboard B".into()))
                        .set_cell("available", Cell::Bool(false))
                        .set_cell("pages", Cell::Int(30)),
                ],
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let metadata = book_view(&orm, "list").await;
        let list = &metadata["views"][0];
        let dashboard = list["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["kind"] == "dashboard")
            .unwrap();
        assert_eq!(dashboard["items"].as_array().unwrap().len(), 3);
        let action_id: i64 = scalar(&orm,"SELECT res_id::text FROM ir_model_data WHERE module='sample_library' AND name='book_action'").await.parse().unwrap();
        let view_id = list["identity"]["id"].as_i64().unwrap();
        let revision = dashboard["revision"].as_str().unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let records = tx.env().records("sample_library.book").unwrap();
        let Exposure::Exposed(method) = tx
            .exposure()
            .get(records.model_id(), "erp_read_dashboard")
            .unwrap()
            .clone()
        else {
            panic!("dashboard is exposed")
        };
        let args = |view_id, revision: &str| {
            erp_fields::ValueMap::from_iter([
                ("action_id".into(), Value::Int(action_id)),
                ("view_id".into(), Value::Int(view_id)),
                ("revision".into(), Value::str(revision)),
                ("period".into(), Value::str("last_7_days")),
                (
                    "domain_scope".into(),
                    Value::List(
                        vec![Value::List(
                            vec![
                                Value::str("title"),
                                Value::str("="),
                                Value::str("Dashboard A"),
                            ]
                            .into(),
                        )]
                        .into(),
                    ),
                ),
            ])
        };
        let value = method
            .call(
                &mut tx,
                records.clone(),
                RpcArgs::new(args(view_id, revision)),
            )
            .await
            .unwrap();
        let answer = erp_fields::convert::value_to_json(&value).unwrap();
        let values = answer["values"].as_array().unwrap();
        assert_eq!(
            values.iter().find(|v| v["name"] == "available").unwrap()["value"],
            "1"
        );
        assert_eq!(
            values.iter().find(|v| v["name"] == "unavailable").unwrap()["value"],
            "0"
        );
        assert_eq!(
            values.iter().find(|v| v["name"] == "pages_added").unwrap()["value"],
            "12"
        );
        assert_eq!(
            values.iter().find(|v| v["name"] == "pages_added").unwrap()["change"],
            serde_json::Value::Null
        );
        let error = method
            .call(
                &mut tx,
                records.clone(),
                RpcArgs::new(args(view_id, &"0".repeat(64))),
            )
            .await
            .unwrap_err();
        let erp_orm::rpc::BindError::Call(error) = error else {
            panic!("expected method refusal")
        };
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Dashboard declaration changed. Reload this view."
        );
        let error = method
            .call(&mut tx, records, RpcArgs::new(args(view_id + 1, revision)))
            .await
            .unwrap_err();
        let erp_orm::rpc::BindError::Call(error) = error else {
            panic!("expected method refusal")
        };
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Dashboard view does not belong to the selected action."
        );
        let today = erp_fields::Date::from_ymd(2024, 3, 10).unwrap();
        let (current, previous) = erp_addon_web::test_dashboard_ranges(
            &mut tx,
            "rolling",
            Some(7),
            "America/New_York",
            true,
            today,
            366,
        )
        .await
        .unwrap();
        assert_eq!(
            current,
            (
                "2024-03-04 05:00:00".to_owned(),
                "2024-03-11 04:00:00".to_owned()
            )
        );
        assert_eq!(
            previous,
            (
                "2024-02-26 05:00:00".to_owned(),
                "2024-03-04 05:00:00".to_owned()
            )
        );
        let (current, previous) = erp_addon_web::test_dashboard_ranges(
            &mut tx,
            "month",
            None,
            "UTC",
            false,
            erp_fields::Date::from_ymd(2024, 3, 31).unwrap(),
            366,
        )
        .await
        .unwrap();
        assert_eq!(current, ("2024-03-01".to_owned(), "2024-04-01".to_owned()));
        assert_eq!(previous, ("2024-02-01".to_owned(), "2024-03-01".to_owned()));
        let error =
            erp_addon_web::test_dashboard_ranges(&mut tx, "month", None, "UTC", false, today, 1)
                .await
                .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Dashboard period exceeds web.dashboard_max_period_days."
        );
        let error = erp_addon_web::test_dashboard_ranges(
            &mut tx,
            "month",
            None,
            "Not/A_Timezone",
            false,
            today,
            366,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Dashboard timezone 'Not/A_Timezone' is not recognized."
        );
        tx.rollback().await.unwrap();
        drop(tx);
        let mut setup = orm.begin(SUPERUSER_ID).await.unwrap();
        let company: i64 = setup
            .query("SELECT id FROM res_company ORDER BY id LIMIT 1", &[])
            .await
            .unwrap()[0]
            .get(0);
        let group: i64 = setup
            .query(
                "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_user'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let model_id: i64 = setup
            .query(
                "SELECT id FROM ir_model WHERE model='sample_library.book'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let partner = setup
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut setup,
                vec![RawVals::new().set_cell("name", Cell::Str("Dashboard reader".into()))],
            )
            .await
            .unwrap();
        let admin_group: i64 = setup
            .query(
                "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let admin_partner = setup
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut setup,
                vec![RawVals::new().set_cell("name", Cell::Str("Dashboard administrator".into()))],
            )
            .await
            .unwrap();
        setup
            .env()
            .records("res.users")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Dashboard administrator".into()))
                        .set_cell("login", Cell::Str("dashboard-administrator".into()))
                        .set_cell("partner_id", Cell::Id(admin_partner.ids()[0]))
                        .set_cell("company_id", Cell::Id(company))
                        .set_commands(
                            "company_ids",
                            [erp_orm::Command::set([erp_fields::RecId::from_real(
                                company,
                            )
                            .unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [erp_orm::Command::set([erp_fields::RecId::from_real(
                                admin_group,
                            )
                            .unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        let user = setup
            .env()
            .records("res.users")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Dashboard reader".into()))
                        .set_cell("login", Cell::Str("dashboard-reader".into()))
                        .set_cell("partner_id", Cell::Id(partner.ids()[0]))
                        .set_cell("company_id", Cell::Id(company))
                        .set_commands(
                            "company_ids",
                            [erp_orm::Command::set([erp_fields::RecId::from_real(
                                company,
                            )
                            .unwrap()])],
                        )
                        .set_commands(
                            "group_ids",
                            [erp_orm::Command::set([
                                erp_fields::RecId::from_real(group).unwrap()
                            ])],
                        ),
                ],
            )
            .await
            .unwrap();
        let user_id = user.ids()[0];
        setup
            .env()
            .records("ir.access")
            .unwrap()
            .create(
                &mut setup,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Dashboard test restriction".into()))
                        .set_cell("model_id", Cell::Id(model_id))
                        .set_cell("operation", Cell::Str("r".into()))
                        .set_cell("domain", Cell::Str("[('available', '=', True)]".into())),
                ],
            )
            .await
            .unwrap();
        setup.commit().await.unwrap();
        drop(setup);
        let registry = Arc::clone(orm.registry());
        let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(
            erp_security::source::PolicySource::build(Arc::clone(&registry), db.pool().clone())
                .await
                .unwrap(),
        );
        let restricted = Orm::builder(Arc::clone(&registry), db.pool().clone())
            .generation_attachment(erp_orm::GenerationAttachment::new(
                Arc::clone(&registry),
                orm.env(SUPERUSER_ID, erp_orm::Context::new())
                    .generation_attachment::<erp_views::PublishedViewTypes>()
                    .unwrap(),
            ))
            .require_core_tables(erp_security::auth::core_tables())
            .resolver(erp_security::resolver::Resolver::for_registry(&registry).unwrap())
            .policy(provider)
            .impls(erp_modules::base_models::IMPLS.iter().copied())
            .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .build()
            .unwrap();
        let mut user_tx = restricted
            .begin(erp_orm::UserId::new(user_id).unwrap())
            .await
            .unwrap();
        let user_books = user_tx.env().records("sample_library.book").unwrap();
        let Exposure::Exposed(user_method) = user_tx
            .exposure()
            .get(user_books.model_id(), "erp_read_dashboard")
            .unwrap()
            .clone()
        else {
            panic!("dashboard is exposed")
        };
        let mut user_args = args(view_id, revision);
        user_args.insert(
            "domain_scope".to_owned(),
            Value::List(
                vec![Value::List(
                    vec![
                        Value::str("title"),
                        Value::str("ilike"),
                        Value::str("Dashboard"),
                    ]
                    .into(),
                )]
                .into(),
            ),
        );
        let answer = user_method
            .call(&mut user_tx, user_books, RpcArgs::new(user_args))
            .await
            .unwrap();
        let answer = erp_fields::convert::value_to_json(&answer).unwrap();
        let values = answer["values"].as_array().unwrap();
        assert_eq!(
            values.iter().find(|v| v["name"] == "available").unwrap()["value"],
            "1"
        );
        assert_eq!(
            values.iter().find(|v| v["name"] == "unavailable").unwrap()["value"],
            "0"
        );
        assert_eq!(
            values.iter().find(|v| v["name"] == "pages_added").unwrap()["value"],
            "12"
        );
        user_tx.rollback().await.unwrap();
    });
}

/// The two sample reports are installed and offered in the Print menu of their models, and
/// printing one book lays out a PDF named after it whose document carries its title.
#[test]
fn sample_library_reports_are_listed_and_print_a_book() {
    use erp_report::LayoutEngine;
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db, &repo_addons());
        drop(system.initialize(InitOptions::new()).await.unwrap());
        let env = system.system_env(erp_orm::Context::new());
        let orm = system
            .button_immediate_install(&env, &name())
            .await
            .unwrap()
            .orm;
        // Printed as a user of company 1, whose letterhead the layout prints.
        let mut tx = orm
            .begin_with(SUPERUSER_ID, erp_orm::Context::new().with_company_id(1))
            .await
            .unwrap();
        tx.env()
            .records("res.company")
            .unwrap()
            .browse(vec![1])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("street", Cell::Str("Jalan Merdeka 12".into()))
                    .set_cell("street2", Cell::Str("Lantai 3".into()))
                    .set_cell("website", Cell::Str("www.example.com".into()))
                    .set_cell("vat", Cell::Str("01.234.567.8-901.000".into()))
                    .set_cell("zip", Cell::Str("40111".into()))
                    .set_cell("city", Cell::Str("Bandung".into()))
                    .set_cell("phone", Cell::Str("+62 22 555 0100".into()))
                    .set_cell("email", Cell::Str("hello@example.com".into())),
            )
            .await
            .unwrap();
        for (model, reports) in [
            (
                "sample_library.book",
                [
                    ("sample_library.report_book_sheet", "Book Sheet"),
                    ("sample_library.report_book_list", "Book List"),
                ],
            ),
            (
                "sample_library.loan",
                [
                    ("sample_library.report_loan_slip", "Loan Slip"),
                    ("sample_library.report_loan_register", "Loan Register"),
                ],
            ),
        ] {
            let listed = erp_addon_base::report_print::printable(&mut tx, model)
                .await
                .unwrap();
            let expected: Vec<(String, String)> = reports
                .iter()
                .map(|(report, label)| ((*report).to_owned(), (*label).to_owned()))
                .collect();
            assert_eq!(listed, expected, "{model}");
        }
        let author = tx
            .env()
            .records("sample_library.author")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Mira Hale".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let books = tx.env().records("sample_library.book").unwrap();
        let book = books
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("title", Cell::Str("The Printed Atlas".into()))
                        .set_cell("author_id", Cell::Id(author))
                        .set_cell("isbn", Cell::Str("9780000000017".into()))
                        .set_cell("pages", Cell::Int(312))
                        .set_cell("publication_year", Cell::Int(2017))
                        .set_cell("price", Cell::Float(24.5))
                        .set_cell(
                            "summary",
                            Cell::Str("A cartographer maps a city that redraws itself.".into()),
                        ),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let borrower = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Ari Wibowo".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        let date = |d| Cell::Date(erp_fields::Date::from_ymd(2026, 11, d).unwrap());
        let loan = tx
            .env()
            .records("sample_library.loan")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("book_id", Cell::Id(book))
                        .set_cell("borrower_id", Cell::Id(borrower))
                        .set_cell("loan_date", date(2))
                        .set_cell("due_date", date(16))
                        .set_cell("state", Cell::Str("out".into())),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let slip = erp_addon_base::report_print::find(&mut tx, "sample_library.report_loan_slip")
            .await
            .unwrap();
        let slip = erp_addon_base::report_print::prepare(&mut tx, &slip, &[loan])
            .await
            .unwrap();
        // The same slip printed after the company got a logo (the letterhead shows it).
        let logo = test_png(96, 48);
        tx.env()
            .records("res.company")
            .unwrap()
            .browse(vec![1])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("logo", Cell::Bytes(logo.clone().into())),
            )
            .await
            .unwrap();
        let slip_logo = {
            let report =
                erp_addon_base::report_print::find(&mut tx, "sample_library.report_loan_slip")
                    .await
                    .unwrap();
            erp_addon_base::report_print::prepare(&mut tx, &report, &[loan])
                .await
                .unwrap()
        };
        let second = books
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("title", Cell::Str("Harbour Lights".into()))
                        .set_cell("pages", Cell::Int(198))
                        .set_cell("available", Cell::Bool(false)),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let register =
            erp_addon_base::report_print::find(&mut tx, "sample_library.report_loan_register")
                .await
                .unwrap();
        let register = erp_addon_base::report_print::prepare(&mut tx, &register, &[loan])
            .await
            .unwrap();
        let list = erp_addon_base::report_print::find(&mut tx, "sample_library.report_book_list")
            .await
            .unwrap();
        let list = erp_addon_base::report_print::prepare(&mut tx, &list, &[book, second])
            .await
            .unwrap();
        let report =
            erp_addon_base::report_print::find(&mut tx, "sample_library.report_book_sheet")
                .await
                .unwrap();
        erp_addon_base::report_print::check_records(&mut tx, &report, None, &[book])
            .await
            .unwrap();
        let document = erp_addon_base::report_print::prepare(&mut tx, &report, &[book])
            .await
            .unwrap();
        let file = erp_addon_base::report_print::file_name(&mut tx, &report, &[book], "pdf")
            .await
            .unwrap();
        // A direct report address prints no more than the Print menu prints at once, and no id twice.
        erp_addon_base::report_job::check_direct_print(&mut tx, &[book])
            .await
            .unwrap();
        let twice = erp_addon_base::report_job::check_direct_print(&mut tx, &[book, book])
            .await
            .unwrap_err();
        assert!(
            twice.message().contains("listed twice"),
            "{}",
            twice.message()
        );
        let many: Vec<i64> = (1..=51).collect();
        let over = erp_addon_base::report_job::check_direct_print(&mut tx, &many)
            .await
            .unwrap_err();
        assert!(
            over.message()
                .contains(erp_addon_base::report_job::SYNC_MAX_KEY),
            "{}",
            over.message()
        );
        tx.rollback().await.unwrap();
        assert_eq!(file, "Book sheet - The Printed Atlas.pdf");
        let html = erp_report::render_html(&document, &[]).unwrap();
        assert!(html.contains("The Printed Atlas"), "{html}");
        // The sheet prints the ISBN as a code with its digits under it. This one does not end in
        // its EAN check digit, so `auto` prints it as Code 128 instead of refusing the sheet.
        assert!(
            html.contains("aria-label=\"Barcode 9780000000017\""),
            "{html}"
        );
        assert!(html.contains(">9780000000017</span></span>"), "{html}");
        let fonts = repo_addons().join("report_fonts");
        let engine =
            erp_report_typst::TypstEngine::new(&fonts, erp_report::Limits::default()).unwrap();
        let pdf = engine
            .render(&report_frame::through_frame(&document))
            .unwrap();
        assert!(pdf.bytes().starts_with(b"%PDF-"));
        let slip_pdf = engine.render(&report_frame::through_frame(&slip)).unwrap();
        assert!(slip_pdf.bytes().starts_with(b"%PDF-"));
        let slip_html = erp_report::render_html(&slip, &[]).unwrap();
        assert!(slip_html.contains("Ari Wibowo"), "{slip_html}");
        // The letterhead carries the company's address and the footer its contact line.
        assert!(slip_html.contains("Bandung"), "{slip_html}");
        assert!(slip_html.contains("hello@example.com"), "{slip_html}");
        for line in [
            "Lantai 3",
            "www.example.com",
            "Tax ID",
            "01.234.567.8-901.000",
        ] {
            assert!(slip_html.contains(line), "{line}: {slip_html}");
        }
        // Without a logo the letterhead has no image, in the preview or the PDF.
        assert!(!slip_html.contains("role=\"img\""), "{slip_html}");
        assert!(pdf_images(slip_pdf.bytes()).is_empty());
        // With the logo the preview has the image box and the PDF carries the raster, at the
        // size of the stored image.
        let logo_html = erp_report::render_html(&slip_logo, &[]).unwrap();
        assert_eq!(logo_html.matches("role=\"img\"").count(), 1, "{logo_html}");
        assert!(logo_html.contains("aria-label=\"Logo\""), "{logo_html}");
        assert!(logo_html.contains("Bandung"), "{logo_html}");
        let logo_pdf = engine
            .render(&report_frame::through_frame(&slip_logo))
            .unwrap();
        assert_eq!(pdf_images(logo_pdf.bytes()), vec![(96, 48)]);
        // The same two slips through `erp report-worker` processes, as the server prints them
        // (`report_print.rs`): the document crosses the frame protocol, so the logo reaches the
        // worker as a picture node and its PDF carries the raster. A print in this process
        // (above) never showed a picture the protocol could not carry.
        let pool = erp_report_worker::WorkerPool::new(
            erp_report_worker::PoolSettings::new(
                erp_report_worker::WorkerCommand::new(env!("CARGO_BIN_EXE_erp"), ["report-worker"]),
                fonts.clone(),
            )
            .with(erp_report_worker::Setting::Workers, 1)
            .unwrap(),
        )
        .unwrap();
        let worker_logo_pdf = pool.render(&slip_logo).unwrap();
        assert!(worker_logo_pdf.bytes().starts_with(b"%PDF-"));
        assert_eq!(pdf_images(worker_logo_pdf.bytes()), vec![(96, 48)]);
        let worker_slip_pdf = pool.render(&slip).unwrap();
        assert!(pdf_images(worker_slip_pdf.bytes()).is_empty());
        drop(pool);
        // The loan register prints the loan in one table.
        let register_html = erp_report::render_html(&register, &[]).unwrap();
        assert!(
            register_html.contains("Loan Register") && register_html.contains("Ari Wibowo"),
            "{register_html}"
        );
        assert!(
            engine
                .render(&report_frame::through_frame(&register))
                .unwrap()
                .bytes()
                .starts_with(b"%PDF-")
        );
        // The book list prints both books in one table.
        let list_html = erp_report::render_html(&list, &[]).unwrap();
        assert!(
            list_html.contains("The Printed Atlas")
                && list_html.contains("Harbour Lights")
                && list_html.contains("2 books"),
            "{list_html}"
        );
        assert_eq!(list_html.matches("<table").count(), 1, "{list_html}");
        let list_pdf = engine.render(&report_frame::through_frame(&list)).unwrap();
        assert!(list_pdf.bytes().starts_with(b"%PDF-"));
        // The two sample prints, kept in the test scratch folder for a look at the layout.
        let out = Path::new(env!("CARGO_TARGET_TMPDIR"));
        fs::write(out.join("sample-book-sheet.pdf"), pdf.bytes()).unwrap();
        fs::write(out.join("sample-loan-slip.pdf"), slip_pdf.bytes()).unwrap();
        fs::write(out.join("sample-loan-slip-logo.pdf"), logo_pdf.bytes()).unwrap();
        fs::write(
            out.join("sample-loan-slip-logo-worker.pdf"),
            worker_logo_pdf.bytes(),
        )
        .unwrap();
        fs::write(out.join("sample-book-list.pdf"), list_pdf.bytes()).unwrap();
    });
}

/// Image XObjects of a PDF as (width, height).
fn pdf_images(bytes: &[u8]) -> Vec<(i64, i64)> {
    let pdf = lopdf::Document::load_mem(bytes).unwrap();
    pdf.objects
        .values()
        .filter_map(|object| object.as_stream().ok())
        .filter(|s| s.dict.get(b"Subtype").and_then(|n| n.as_name()).ok() == Some(&b"Image"[..]))
        .map(|s| {
            let size = |key: &[u8]| s.dict.get(key).unwrap().as_i64().unwrap();
            (size(b"Width"), size(b"Height"))
        })
        .collect()
}

/// A valid RGB PNG of one colour (stored zlib blocks), for a company logo.
fn test_png(width: u32, height: u32) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend(kind);
        out.extend(data);
        let mut crc = !0u32;
        for b in &out[start..] {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        out.extend((!crc).to_be_bytes());
    }
    let row: Vec<u8> = std::iter::once(0)
        .chain([30, 90, 160].repeat(width as usize))
        .collect();
    let raw = row.repeat(height as usize);
    let mut z = vec![0x78, 0x01];
    for (i, block) in raw.chunks(65_535).enumerate() {
        z.push(u8::from((i + 1) * 65_535 >= raw.len()));
        let len = block.len() as u16;
        z.extend(len.to_le_bytes());
        z.extend((!len).to_le_bytes());
        z.extend(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = width.to_be_bytes().to_vec();
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}
