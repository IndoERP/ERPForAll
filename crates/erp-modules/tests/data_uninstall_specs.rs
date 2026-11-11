//! Uninstalling modules that loaded data: hooks first, then the records the modules own
//! in models that stay, then the schema, with the default
//! data loader of the module system.
//!
//! Dev-folder addons, a throwaway database on the dev cluster, leaked model declarations
//! (`common::model`). Each test runs in its own process (nextest).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use common::*;
use erp_modules::error::Error;
use erp_modules::hooks::{HookImpl, HookKind};
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_orm::{BoxFuture, Orm, SUPERUSER_ID, Transaction};
use erp_testkit::TestDb;

fn uninstall_b(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        // the records b owns in a's model are still there when its hook runs
        let rows = tx
            .query(
                "SELECT count(*) FROM test_a a WHERE EXISTS \
                 (SELECT 1 FROM ir_model_data d WHERE d.model = 'test.a' AND d.res_id = a.id \
                  AND d.module = 'b')",
                &[],
            )
            .await?;
        let count: i64 = rows[0].get(0);
        log(format!("uninstall b owned={count}"));
        Ok(())
    })
}

fn uninstall_d(tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move {
        let rows = tx.query("SELECT count(*) FROM test_d", &[]).await?;
        let count: i64 = rows[0].get(0);
        log(format!("uninstall d rows={count}"));
        Ok(())
    })
}

static UNINSTALL_B: HookImpl =
    HookImpl::new("b", HookKind::Uninstall, "uninstall_b", uninstall_b).at(file!(), line!());
static UNINSTALL_D: HookImpl =
    HookImpl::new("d", HookKind::Uninstall, "uninstall_d", uninstall_d).at(file!(), line!());

/// The five-module fixture with `b` and `d` hooks, `a`'s model, `b`'s extension of it and `b`'s,
/// `d`'s and `e`'s own models, everything installed.
async fn fixture(db: &TestDb, addons: &Addons) -> (ModuleSystem, Orm) {
    spec_fixture(addons);
    addons
        .add(
            "b",
            "name = 'B'\ndepends = ['a']\nuninstall_hook = 'uninstall_b'\n",
        )
        .add(
            "d",
            "name = 'D'\ndepends = ['b', 'c']\nuninstall_hook = 'uninstall_d'\n",
        );
    let system = builder(db, addons)
        .decls([
            model("a", "test.a"),
            model("b", "test.b"),
            extension("b", "test.a", "b_field"),
            model("d", "test.d"),
            model("e", "test.e"),
        ])
        .hooks([&UNINSTALL_B, &UNINSTALL_D])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    committed(
        system
            .button_immediate_install(&su(&init.orm), &[n("e")])
            .await
            .unwrap(),
    );
    let orm = system.load().await.unwrap().orm;
    (system, orm)
}

/// Records of `test.a` and the external ids that name them: 1 and 2 belong to `b`, 3 to `d`, 4 to
/// `a` itself, 5 to `b` and `c` together (`c` stays), 6 to `d` and the `__cloc_exclude__` marker.
const SEED: &str = "\
    INSERT INTO test_a (id, name) VALUES (1, 'b one'), (2, 'b two'), (3, 'd one'), \
        (4, 'a own'), (5, 'b and c'), (6, 'd marked'); \
    INSERT INTO test_d (id, name) VALUES (1, 'd own'); \
    INSERT INTO ir_model_data (module, name, model, res_id, noupdate) VALUES \
        ('b', 'rec1', 'test.a', 1, false), ('b', 'rec2', 'test.a', 2, false), \
        ('d', 'rec3', 'test.a', 3, false), ('a', 'rec4', 'test.a', 4, false), \
        ('b', 'rec5', 'test.a', 5, false), ('c', 'rec5', 'test.a', 5, false), \
        ('d', 'rec6', 'test.a', 6, false), ('__cloc_exclude__', 'rec6', 'test.a', 6, false), \
        ('d', 'own', 'test.d', 1, false)";

async fn left_in_a(orm: &Orm) -> Vec<String> {
    rows(orm, "SELECT name FROM test_a ORDER BY id")
        .await
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect()
}

/// modules' records in models that stay, fields, models and tables are removed;
/// the modules end `uninstalled` with an empty `latest_version`; the rebuilt registry has none of
/// their models.
#[tokio::test]
async fn spec_mod_040_uninstall_order_hooks_records_schema_and_states() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let (system, installed) = fixture(&db, &addons).await;
    exec(&installed, SEED).await;
    take_log();

    let outcome = system
        .button_immediate_uninstall(&su(&installed), &[n("b")])
        .await
        .unwrap();

    // hooks: d before b, both with their data in place
    assert_eq!(take_log(), ["uninstall d rows=1", "uninstall b owned=3"]);
    assert_eq!(outcome.uninstalled, vec![n("b"), n("d"), n("e")]);
    for module in ["b", "d", "e"] {
        assert_eq!(
            rows(
                &outcome.orm,
                &format!(
                    "SELECT state, latest_version FROM ir_module_module WHERE name = '{module}'"
                )
            )
            .await[0],
            vec![Some("uninstalled".to_owned()), None],
            "{module}"
        );
    }
    for module in ["a", "c"] {
        assert_eq!(state(&outcome.orm, module).await, "installed");
    }
    // records: what b and d own is gone, a's own record and the one c still names stay
    assert_eq!(left_in_a(&outcome.orm).await, ["a own", "b and c"]);
    // tables and models of the removed modules
    for table in ["test_b", "test_d", "test_e"] {
        assert!(!table_exists(&outcome.orm, table).await, "{table}");
        assert!(
            outcome
                .orm
                .registry()
                .model(&table.replace('_', "."))
                .is_none()
        );
    }
    assert!(table_exists(&outcome.orm, "test_a").await);
    assert!(
        rows(
            &outcome.orm,
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_name = 'test_a' AND column_name = 'b_field'"
        )
        .await
        .is_empty(),
        "b's column on a's model is dropped"
    );
    // external ids: none of the removed modules, the shared record keeps c's
    assert!(
        rows(
            &outcome.orm,
            "SELECT name FROM ir_model_data WHERE module IN ('b', 'd', 'e', '__cloc_exclude__')"
        )
        .await
        .is_empty()
    );
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT module, name FROM ir_model_data WHERE model = 'test.a' ORDER BY res_id"
        )
        .await,
        [
            [Some("a".to_owned()), Some("rec4".to_owned())],
            [Some("c".to_owned()), Some("rec5".to_owned())],
        ]
    );
    db.cleanup().await.unwrap();
}

/// real uninstall, newest external id first; copied views are and selection ondelete policies
/// are)
#[tokio::test]
async fn spec_data_029_uninstall_deletes_owned_records_and_keeps_the_shared_one() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let (system, installed) = fixture(&db, &addons).await;
    exec(&installed, SEED).await;
    // only e is uninstalled by name, but it takes b's dependents with it: uninstall `d`
    let outcome = system
        .button_immediate_uninstall(&su(&installed), &[n("d")])
        .await
        .unwrap();
    assert_eq!(outcome.uninstalled, vec![n("d"), n("e")]);
    // d's records are gone, b's stay (b stays installed)
    assert_eq!(
        left_in_a(&outcome.orm).await,
        ["b one", "b two", "a own", "b and c"]
    );
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT count(*)::text FROM ir_model_data WHERE module = 'd'"
        )
        .await,
        [[Some("0".to_owned())]]
    );
    assert_eq!(state(&outcome.orm, "b").await, "installed");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_undeletable_record_fails_the_uninstall_and_rolls_everything_back() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let (system, installed) = fixture(&db, &addons).await;
    exec(&installed, SEED).await;
    exec(
        &installed,
        "CREATE TABLE keeper (rid bigint NOT NULL REFERENCES test_a(id)); \
         INSERT INTO keeper VALUES (3)",
    )
    .await;
    // what the ORM says about deleting that record
    let mut probe = installed.begin(SUPERUSER_ID).await.unwrap();
    let records = probe
        .env()
        .records("test.a")
        .unwrap()
        .with_context([("force_delete", erp_fields::Value::Bool(true))])
        .browse(vec![3])
        .unwrap();
    let expected = records.unlink(&mut probe).await.unwrap_err();
    drop(probe);

    let err = system
        .button_immediate_uninstall(&su(&installed), &[n("d")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::PurgeFailed { modules, .. } if modules == &[n("d"), n("e")]),
        "{err:?}"
    );
    let messages = chain(&err);
    // the ORM error is followed by the database error it wraps
    assert_eq!(
        messages[..3],
        [
            "Modules d, e: the data loader's purge before uninstall failed".to_owned(),
            "the data loader failed".to_owned(),
            expected.message().to_owned(),
        ]
    );
    assert_eq!(state(&installed, "d").await, "installed");
    assert_eq!(left_in_a(&installed).await.len(), 6);
    assert!(table_exists(&installed, "test_d").await);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_purge_returns_a_notice_for_a_model_that_is_not_available() {
    use erp_data::load::{DataNotice, NoticeKind};
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let (system, installed) = fixture(&db, &addons).await;
    exec(&installed, SEED).await;
    exec(
        &installed,
        "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
         VALUES ('b', 'ghost', 'no.such', 1, false)",
    )
    .await;
    let outcome = system
        .button_immediate_uninstall(&su(&installed), &[n("b")])
        .await
        .unwrap();
    assert_eq!(
        outcome.data_notices,
        [DataNotice::new(
            "b, d, e",
            "",
            NoticeKind::PurgeUnavailableModel {
                model: "no.such".to_owned()
            }
        )]
    );
    db.cleanup().await.unwrap();
}

/// An external id row without a record is not an orphaned record: it does not stop the uninstall,
/// and the final delete of the modules' external ids removes it.
#[tokio::test]
async fn an_external_id_without_a_record_does_not_block_the_uninstall() {
    let addons = Addons::new();
    let db = TestDb::create().await.unwrap();
    let (system, installed) = fixture(&db, &addons).await;
    exec(&installed, SEED).await;
    exec(
        &installed,
        "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
         VALUES ('b', 'no_record', 'test.a', NULL, false)",
    )
    .await;
    let outcome = system
        .button_immediate_uninstall(&su(&installed), &[n("b")])
        .await
        .unwrap();
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT count(*)::text FROM ir_model_data WHERE module IN ('b', 'd', 'e')"
        )
        .await,
        [[Some("0".to_owned())]]
    );
    db.cleanup().await.unwrap();
}

/// A view a staying module names by external id, built on a copied view of the module being
/// uninstalled, is deleted with the copy; the outcome says so with a notice naming the view and
/// the external id (review A4-S1: the purge report was dropped, so the deletion was
/// silent).
#[tokio::test]
async fn a_named_view_removed_with_a_copy_is_noticed() {
    use erp_data::load::{DataNotice, NoticeKind};
    use erp_registry::{FieldDecl, ModelDecl};
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n");
    let fields: &'static [FieldDecl] = Box::leak(
        vec![
            FieldDecl::char("name"),
            FieldDecl::char("key"),
            FieldDecl::many2one("inherit_id", "ir.ui.view").ondelete(erp_fields::OnDelete::Cascade),
        ]
        .into_boxed_slice(),
    );
    let view: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("a", "ir.ui.view")
            .description("View")
            .fields(fields)
            .at(file!(), line!()),
    ));
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).decls([view]).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("b")])
        .await
        .unwrap()
        .orm;
    exec(
        &installed,
        "INSERT INTO ir_ui_view (id, name, key, inherit_id) VALUES \
            (1, 'copy', 'b.copy', NULL), (2, 'named', 'b.named', 1), (3, 'kept', 'b.kept', NULL); \
         INSERT INTO ir_model_data (module, name, model, res_id, noupdate) VALUES \
            ('a', 'view_named', 'ir.ui.view', 2, false), ('a', 'view_kept', 'ir.ui.view', 3, false)",
    )
    .await;
    let outcome = system
        .button_immediate_uninstall(&su(&installed), &[n("b")])
        .await
        .unwrap();
    assert_eq!(
        outcome.data_notices,
        [DataNotice::new(
            "b",
            "",
            NoticeKind::PurgeCascadedNamedView {
                view_id: 2,
                xmlid: "a.view_named".to_owned()
            }
        )]
    );
    assert_eq!(
        rows(&outcome.orm, "SELECT id::text FROM ir_ui_view ORDER BY id").await,
        [[Some("3".to_owned())]]
    );
    db.cleanup().await.unwrap();
}

/// A view a staying module names, built on a view the uninstalled module owns, is deleted with
/// it; the outcome says so with a notice naming the view and the external id (review S-4 of
/// : only the views taken with a copy were noticed).
#[tokio::test]
async fn a_named_view_removed_with_a_removed_modules_view_is_noticed() {
    use erp_data::load::{DataNotice, NoticeKind};
    use erp_registry::{FieldDecl, ModelDecl};
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n");
    let fields: &'static [FieldDecl] = Box::leak(
        vec![
            FieldDecl::char("name"),
            FieldDecl::char("key"),
            FieldDecl::many2one("inherit_id", "ir.ui.view").ondelete(erp_fields::OnDelete::Cascade),
        ]
        .into_boxed_slice(),
    );
    let view: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("a", "ir.ui.view")
            .description("View")
            .fields(fields)
            .at(file!(), line!()),
    ));
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).decls([view]).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("b")])
        .await
        .unwrap()
        .orm;
    exec(
        &installed,
        "INSERT INTO ir_ui_view (id, name, key, inherit_id) VALUES \
            (1, 'own', NULL, NULL), (2, 'named', NULL, 1), (3, 'kept', NULL, NULL); \
         INSERT INTO ir_model_data (module, name, model, res_id, noupdate) VALUES \
            ('b', 'view_own', 'ir.ui.view', 1, false), \
            ('a', 'view_named', 'ir.ui.view', 2, false), ('a', 'view_kept', 'ir.ui.view', 3, false)",
    )
    .await;
    let outcome = system
        .button_immediate_uninstall(&su(&installed), &[n("b")])
        .await
        .unwrap();
    assert_eq!(
        outcome.data_notices,
        [DataNotice::new(
            "b",
            "",
            NoticeKind::PurgeViewCascaded {
                view_id: 2,
                xmlid: "a.view_named".to_owned()
            }
        )]
    );
    assert_eq!(
        rows(&outcome.orm, "SELECT id::text FROM ir_ui_view ORDER BY id").await,
        [[Some("3".to_owned())]]
    );
    db.cleanup().await.unwrap();
}

/// A view model with a table but no stored `key` cannot tell the copies of a module's views: the
/// uninstall fails saying so, instead of purging no copy without a word (review F-5).
#[tokio::test]
async fn a_view_table_without_a_stored_key_fails_the_uninstall() {
    let addons = Addons::new();
    addons
        .add("a", "name = 'A'\n")
        .add("b", "name = 'B'\ndepends = ['a']\n");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("a", "ir.ui.view")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("b")])
        .await
        .unwrap()
        .orm;
    let err = system
        .button_immediate_uninstall(&su(&installed), &[n("b")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::PurgeFailed { modules, .. } if modules == &[n("b")]),
        "{err:?}"
    );
    assert_eq!(
        chain(&err),
        [
            "Modules b: the data loader's purge before uninstall failed",
            "the data loader failed",
            "ir.ui.view has a table but no stored 'key' field: the views copied from the \
             removed modules' views cannot be found",
        ]
    );
    let erp_modules::error::Error::PurgeFailed { source, .. } = &err else {
        unreachable!()
    };
    let Error::DataLoad(load) = &**source else {
        panic!("{source:?}")
    };
    let erp_data::load::LoadErrorKind::Orm(inner) = load.kind() else {
        panic!("{load:?}")
    };
    assert_eq!(inner.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(state(&installed, "b").await, "installed");
    db.cleanup().await.unwrap();
}
