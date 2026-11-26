//! A real addon operation exercises protection through the loader, PostgreSQL and the ORM.

#![allow(clippy::unwrap_used, clippy::too_many_lines, reason = "test fixture")]

mod common;

use common::*;
use erp_fields::{Error, ErrorKind, OnDelete};
use erp_modules::system::InitOptions;
use erp_orm::{Orm, SUPERUSER_ID};
use erp_registry::{FieldDecl, ModelDecl};
use erp_testkit::TestDb;

const REF: ModelDecl = ModelDecl::new("catalog", "test.ref").fields(&[
    FieldDecl::char("name"),
    FieldDecl::many2one("parent_id", "test.parent").ondelete(OnDelete::Cascade),
]);

const V1: &str = r#"<erp>
    <record id="parent" model="test.parent"/>
    <record id="plain" model="test.ref"/>
    <record id="obsolete" model="test.ref" protect="1"/>
    <record id="shared" model="test.ref" protect="1">
        <field name="parent_id" ref="parent"/>
    </record>
    <record id="race_parent" model="test.parent"/>
    <record id="race" model="test.ref">
        <field name="parent_id" ref="race_parent"/>
    </record>
    <record id="local" model="test.local" protect="1"/>
    <data noupdate="1">
        <record id="late" model="test.ref"><field name="name">original</field></record>
        <record id="toggle" model="test.ref" protect="1"/>
    </data>
</erp>"#;

const V2: &str = r#"<erp>
    <record id="parent" model="test.parent"/>
    <record id="shared" model="test.ref" protect="1">
        <field name="parent_id" ref="parent"/>
    </record>
    <record id="local" model="test.local" protect="1"/>
    <data noupdate="1">
        <record id="late" model="test.ref" protect="1"><field name="name">ignored</field></record>
        <record id="toggle" model="test.ref" protect="0"/>
    </data>
</erp>"#;

const REFUSAL: &str = "An addon protects this record from deletion or external ID changes.";

fn protected(error: &Error) {
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), REFUSAL);
}

async fn id(orm: &Orm, xid: &str) -> i64 {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let xid = erp_data::XmlId::parse(xid).unwrap();
    let env = tx.env();
    let id = erp_data::model_data::xmlid_to_res_id(&mut tx, &env, &xid)
        .await
        .unwrap()
        .unwrap()
        .1;
    tx.rollback().await.unwrap();
    id
}

async fn unlink(orm: &Orm, model: &str, id: i64) -> Result<(), Error> {
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    let record = tx.env().records(model)?.browse([id])?;
    match record.unlink(&mut tx).await {
        Ok(()) => tx.commit().await,
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

#[tokio::test]
async fn addon_protection_survives_delete_races_and_releases_only_its_owners() {
    let addons = Addons::new();
    addons
        .add("catalog", "name = 'Catalog'\n")
        .add(
            "owner_a",
            "name = 'A'\ndepends = ['catalog']\ndata = ['data.xml']\n",
        )
        .add(
            "owner_b",
            "name = 'B'\ndepends = ['catalog']\ndata = ['data.xml']\n",
        )
        .file("owner_a", "data.xml", V1)
        .file("owner_b", "data.xml", "<erp/>");
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons)
        .decls([
            &REF,
            model("catalog", "test.parent"),
            model("owner_a", "test.local"),
        ])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("owner_a"), n("owner_b")])
        .await
        .unwrap();
    let orm = &installed.orm;
    let shared = id(orm, "owner_a.shared").await;
    let parent = id(orm, "owner_a.parent").await;
    let obsolete = id(orm, "owner_a.obsolete").await;
    let race = id(orm, "owner_a.race").await;
    let race_parent = id(orm, "owner_a.race_parent").await;
    let toggle = id(orm, "owner_a.toggle").await;
    unlink(orm, "test.ref", id(orm, "owner_a.plain").await)
        .await
        .unwrap();
    protected(&unlink(orm, "test.ref", shared).await.unwrap_err());
    protected(&unlink(orm, "test.parent", parent).await.unwrap_err());

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let xid = erp_data::XmlId::parse("owner_a.shared").unwrap();
    let alias = erp_data::model_data::lookup(&mut tx, &env, std::slice::from_ref(&xid))
        .await
        .unwrap()[&xid]
        .id;
    let metadata = env
        .records("ir.model.data")
        .unwrap()
        .browse([alias])
        .unwrap();
    protected(
        &tx.savepoint(async |tx| metadata.unlink(tx).await)
            .await
            .unwrap_err(),
    );
    protected(
        &tx.savepoint(async |tx| {
            tx.execute(
                "UPDATE ir_model_data SET noupdate = true WHERE id = $1",
                &[&alias],
            )
            .await?;
            Ok(())
        })
        .await
        .unwrap_err(),
    );
    protected(
        &tx.savepoint(async |tx| {
            erp_data::model_data::update_xmlids(
                tx,
                &env,
                &[erp_data::XidBinding::new(xid, "test.ref", obsolete, false)],
                false,
            )
            .await
        })
        .await
        .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // Take the cascade snapshot before another transaction establishes protection on its child.
    let mut deleting = orm.begin(SUPERUSER_ID).await.unwrap();
    deleting
        .query("SELECT id FROM test_ref", &[])
        .await
        .unwrap();
    exec(
        orm,
        "INSERT INTO \"erp_record_protection.binding\" (xid_id, module, name, model, res_id, noupdate) \
         SELECT id, module, name, model, res_id, noupdate FROM ir_model_data \
         WHERE module = 'owner_a' AND name = 'race'",
    )
    .await;
    let race_record = deleting
        .env()
        .records("test.parent")
        .unwrap()
        .browse([race_parent])
        .unwrap();
    let error = race_record.unlink(&mut deleting).await.unwrap_err();
    if error.kind() != ErrorKind::Concurrency {
        protected(&error);
    }
    deleting.rollback().await.unwrap();
    drop(deleting);
    protected(&unlink(orm, "test.parent", race_parent).await.unwrap_err());

    // A second owner may protect its own binding to the same record.
    exec(
        orm,
        "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
         SELECT 'owner_b', 'shared', model, res_id, false FROM ir_model_data \
         WHERE module = 'owner_a' AND name = 'shared'",
    )
    .await;
    addons.file(
        "owner_b",
        "data.xml",
        r#"<erp><record id="owner_a.shared" model="test.ref" protect="0"/></erp>"#,
    );
    enroll_fixture_transition(&system, &["owner_b"]).await;
    let foreign = system
        .button_immediate_upgrade(&su(orm), &[n("owner_b")])
        .await
        .unwrap_err();
    assert!(
        chain(&foreign)
            .join("\n")
            .contains("can only change protection on external IDs it owns")
    );
    protected(&unlink(orm, "test.ref", shared).await.unwrap_err());

    addons.file(
        "owner_b",
        "data.xml",
        r#"<erp><record id="shared" model="test.ref" protect="1"/></erp>"#,
    );
    addons.file("owner_a", "data.xml", V2);
    enroll_fixture_transition(&system, &["owner_a", "owner_b"]).await;
    let upgraded = system
        .button_immediate_upgrade(&su(orm), &[n("owner_a"), n("owner_b")])
        .await
        .unwrap();
    let orm = &upgraded.orm;
    assert!(
        rows(
            orm,
            "SELECT name FROM ir_model_data WHERE module = 'owner_a' \
             AND name IN ('obsolete', 'race', 'race_parent')"
        )
        .await
        .is_empty()
    );
    assert!(
        rows(
            orm,
            &format!("SELECT id::text FROM test_ref WHERE id IN ({obsolete}, {race})")
        )
        .await
        .is_empty()
    );
    assert!(
        rows(
            orm,
            &format!("SELECT id::text FROM test_parent WHERE id = {race_parent}")
        )
        .await
        .is_empty()
    );
    let late = id(orm, "owner_a.late").await;
    assert_eq!(
        rows(orm, &format!("SELECT name FROM test_ref WHERE id = {late}")).await,
        [[Some("original".to_owned())]]
    );
    protected(&unlink(orm, "test.ref", late).await.unwrap_err());
    unlink(orm, "test.ref", toggle).await.unwrap();

    // The shared row is a cascade child of A's parent. The first uninstall cannot force
    // that cascade, even though it has temporarily released A's own bindings.
    let refused = system
        .button_immediate_uninstall(&su(orm), &[n("owner_a")])
        .await
        .unwrap_err();
    assert!(chain(&refused).join("\n").contains(REFUSAL));
    assert_eq!(state(orm, "owner_a").await, "installed");
    protected(&unlink(orm, "test.ref", late).await.unwrap_err());
    exec(
        orm,
        &format!("UPDATE test_ref SET parent_id = NULL WHERE id = {shared}"),
    )
    .await;
    let first = system
        .button_immediate_uninstall(&su(orm), &[n("owner_a")])
        .await
        .unwrap();
    assert!(!table_exists(&first.orm, "test_local").await);
    assert_eq!(id(&first.orm, "owner_b.shared").await, shared);
    protected(&unlink(&first.orm, "test.ref", shared).await.unwrap_err());
    let second = system
        .button_immediate_uninstall(&su(&first.orm), &[n("owner_b")])
        .await
        .unwrap();
    assert!(
        rows(&second.orm, "SELECT id::text FROM test_ref")
            .await
            .is_empty()
    );
    db.cleanup().await.unwrap();
}
