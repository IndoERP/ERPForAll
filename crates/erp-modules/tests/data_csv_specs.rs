//! CSV data files through the module system: and.
//!
//! Dev-folder addons, a throwaway database on the dev cluster, leaked model declarations. Each
//! test runs in its own process (nextest).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use common::*;
use erp_data::load::NoticeKind;
use erp_modules::error::{Error, ModuleStep};
use erp_modules::system::InitOptions;
use erp_registry::{FieldDecl, ModelDecl};
use erp_testkit::TestDb;

/// `test.item`: a name and a many2one to `test.cat` (the `model_id:id` column of the spec).
fn item_model() -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(
        vec![
            FieldDecl::char("name"),
            FieldDecl::many2one("cat_id", "test.cat"),
        ]
        .into_boxed_slice(),
    );
    Box::leak(Box::new(
        ModelDecl::new("m", "test.item")
            .description("Test item")
            .fields(fields)
            .at(file!(), line!()),
    ))
}

async fn names(orm: &erp_orm::Orm, table: &str) -> Vec<String> {
    rows(orm, &format!("SELECT name FROM {table} ORDER BY id"))
        .await
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect()
}

#[tokio::test]
async fn spec_data_025_csv_rows_load_into_the_model_of_the_file_name() {
    let addons = Addons::new();
    addons
        .add(
            "m",
            "name = 'M'\ndata = ['data/test.cat-extra.csv', 'data/test.item.csv']\n",
        )
        // a blank line is ignored (a translation column is refused until)
        .file(
            "m",
            "data/test.cat-extra.csv",
            "id,name\ncat_a,A\n\ncat_b,B\n",
        )
        .file(
            "m",
            "data/test.item.csv",
            "id,name,cat_id:id\nitem_1,I1,cat_a\nitem_2,I2,m.cat_b\n",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("m", "test.cat"), item_model()])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("m")])
        .await
        .unwrap();
    assert!(
        outcome.data_notices.is_empty(),
        "{:?}",
        outcome.data_notices
    );
    let orm = outcome.orm;

    // `test.cat-extra.csv` loads into `test.cat`; the blank line is gone
    assert_eq!(names(&orm, "test_cat").await, ["A", "B"]);
    // `cat_id:id` resolved the external ids, qualified and not
    let linked = rows(
        &orm,
        "SELECT i.name, c.name FROM test_item i JOIN test_cat c ON c.id = i.cat_id ORDER BY i.id",
    )
    .await;
    assert_eq!(
        linked,
        [
            [Some("I1".to_owned()), Some("A".to_owned())],
            [Some("I2".to_owned()), Some("B".to_owned())],
        ]
    );
    // every row got its external id in the module
    let xids = rows(
        &orm,
        "SELECT name, model FROM ir_model_data WHERE module = 'm' ORDER BY name",
    )
    .await;
    assert_eq!(
        xids,
        [
            ["cat_a", "test.cat"],
            ["cat_b", "test.cat"],
            ["item_1", "test.item"],
            ["item_2", "test.item"],
        ]
        .map(|[name, model]| [Some(name.to_owned()), Some(model.to_owned())])
    );
    db.cleanup().await.unwrap();
}

/// An invalid row aborts the install with the file's messages.
#[tokio::test]
async fn an_invalid_csv_row_aborts_the_install_with_references_text() {
    let addons = Addons::new();
    addons
        .add(
            "m",
            "name = 'M'\ndata = ['data/test.cat.csv', 'data/test.item.csv']\n",
        )
        .file("m", "data/test.cat.csv", "id,name\ncat_a,A\n")
        .file(
            "m",
            "data/test.item.csv",
            "id,name,cat_id:id\nitem_1,I1,cat_a\nitem_2,I2,nope\n",
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("m", "test.cat"), item_model()])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let before = snapshot(&init.orm).await;
    let err = system
        .button_immediate_install(&su(&init.orm), &[n("m")])
        .await
        .unwrap_err();
    let (module, step, file, source) = step_failed(&err);
    assert_eq!(
        (module, step, file),
        ("m", ModuleStep::DataFile, Some("data/test.item.csv"))
    );
    assert!(matches!(source, Error::DataLoad(_)), "{source:?}");
    let texts = chain(&err);
    assert_eq!(
        texts[0],
        "Module \"m\": data file 'data/test.item.csv' failed"
    );
    assert_eq!(
        texts[1],
        "while parsing data/test.item.csv:1, somewhere inside\n\
         CSV file data/test.item.csv (model test.item)\n\
         Module loading m failed: file data/test.item.csv could not be processed:\n\
         No matching record found for external id 'nope' in field 'Cat'"
    );
    assert_eq!(state(&init.orm, "m").await, "uninstalled");
    assert_eq!(snapshot(&init.orm).await, before, "nothing stays");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_data_026_csv_without_id_in_update_mode_is_skipped_and_the_upgrade_goes_on() {
    let addons = Addons::new();
    addons
        .add(
            "x",
            "name = 'X'\ndata = ['data/test.cat.csv', 'data/test.cat-noid.csv', 'data/a.xml']\n",
        )
        .file("x", "data/test.cat.csv", "id,name\ncat_a,A\n")
        .file("x", "data/test.cat-noid.csv", "name\nno id\n")
        .file("x", "data/a.xml", "<odoo></odoo>");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([model("x", "test.cat")])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    // install is init mode: the file without `id` loads (rows without an external id)
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("x")])
        .await
        .unwrap();
    assert!(
        installed.data_notices.is_empty(),
        "{:?}",
        installed.data_notices
    );
    assert_eq!(names(&installed.orm, "test_cat").await, ["A", "no id"]);

    // the next version changes the file with ids, adds a row, and the XML file follows the CSVs
    addons
        .file("x", "data/test.cat.csv", "id,name\ncat_a,A2\ncat_b,B\n")
        .file("x", "data/test.cat-noid.csv", "name\nchanged but skipped\n")
        .file(
            "x",
            "data/a.xml",
            "<odoo><record id=\"r1\" model=\"test.cat\"><field name=\"name\">from xml</field></record></odoo>",
        );
    enroll_fixture_transition(&system, &["x"]).await;
    let outcome = system
        .button_immediate_upgrade(&su(&installed.orm), &[n("x")])
        .await
        .unwrap();
    // the rows with ids update and create, the file without `id` is skipped untouched, and the
    // files after it still load
    assert_eq!(
        names(&outcome.orm, "test_cat").await,
        ["A2", "no id", "B", "from xml"]
    );
    let [notice] = outcome.data_notices.as_slice() else {
        panic!("expected one notice, got {:?}", outcome.data_notices);
    };
    assert_eq!(
        (notice.module(), notice.file()),
        ("x", "data/test.cat-noid.csv")
    );
    assert_eq!(
        notice.kind().clone(),
        NoticeKind::SkippedFile {
            reason: "data/test.cat-noid.csv: import specification does not contain 'id' and the \
                     module is being updated; file skipped"
                .to_owned()
        }
    );
    assert_eq!(outcome.upgraded, vec![n("x")], "the operation went on");
    db.cleanup().await.unwrap();
}
