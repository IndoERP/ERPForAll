// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real native pre-migration and later-failure rollback from a scalar addon release.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod common;
use common::*;
use erp_modules::artifact_identity::ArtifactSnapshot;
use erp_modules::system::{InitOptions, ModuleSystemBuilder};
use erp_registry::{FieldDecl, ModelDecl};
use erp_testkit::TestDb;
use std::collections::BTreeMap;

mod addon {
    use erp_sdk::migrations::{CompanyMigrationPolicy, SemverVersion};
    use erp_sdk::{Result, Transaction};
    #[erp_sdk::migration(addon="property_upgrade",version="1.1",stage=pre)]
    pub async fn assign(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
        tx.migrate_company_dependent(
            "upgrade.property",
            "note",
            &CompanyMigrationPolicy::companies([1])?,
        )
        .await
    }
}
fn declaration(company: bool) -> &'static ModelDecl {
    Box::leak(Box::new(
        ModelDecl::new("property_upgrade", "upgrade.property")
            .table("upgrade_property")
            .fields(Box::leak(
                vec![FieldDecl::char("note").company_dependent(company)].into_boxed_slice(),
            )),
    ))
}
fn configured(
    db: &TestDb,
    addons: &Addons,
    company: bool,
    previous: BTreeMap<erp_modules::manifest::ModuleName, String>,
) -> ModuleSystemBuilder {
    // The fixture base declares the company table used for explicit owner validation.
    let company_model = Box::leak(Box::new(
        ModelDecl::new("base", "res.company")
            .table("res_company")
            .fields(const { &[FieldDecl::char("name")] }),
    ));
    builder(db, addons)
        .identity_previous(previous)
        .decls([declaration(company), company_model])
}

#[tokio::test]
async fn genuine_addon_upgrade_is_atomic_and_repeatable() {
    for stage in ["post", "end"] {
        let addons = Addons::new();
        addons.add(
            "property_upgrade",
            "name='Property'\nversion='1.0'\ndepends=['base']\n",
        );
        let previous = std::fs::read_dir(addons.path())
            .unwrap()
            .map(|entry| {
                let e = entry.unwrap();
                let name = e.file_name().into_string().unwrap();
                let snap =
                    ArtifactSnapshot::capture(&name, &e.path().canonicalize().unwrap()).unwrap();
                (n(&name), snap.digest().to_owned())
            })
            .collect::<BTreeMap<_, _>>();
        let db = TestDb::create().await.unwrap();
        let system = configured(&db, &addons, false, previous.clone())
            .build()
            .unwrap();
        let initial = system
            .initialize(InitOptions::new().with_skip_auto_install(true))
            .await
            .unwrap();
        let installed = system
            .button_immediate_install(&su(&initial.orm), &[n("property_upgrade")])
            .await
            .unwrap();
        let mut tx = installed.orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        tx.batch("INSERT INTO res_company(id,name) VALUES(1,'One'); INSERT INTO upgrade_property(id,note) VALUES(1,'legacy'); COMMENT ON COLUMN upgrade_property.note IS 'old comment'; CREATE INDEX preserved_id ON upgrade_property(id)").await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        addons.add(
            "property_upgrade",
            "name='Property'\nversion='1.1'\ndepends=['base']\n",
        );
        addons.file(
            "property_upgrade",
            &format!("migrations/1.1/{stage}-failure.sql"),
            "SELECT 1/0;",
        );
        let target = configured(&db, &addons, true, previous.clone())
            .build()
            .unwrap();
        let error = target
            .button_immediate_upgrade(
                &target.system_env(Default::default()),
                &[n("property_upgrade")],
            )
            .await
            .unwrap_err();
        let expected_stage = if stage == "post" {
            erp_modules::migrations::MigrationStage::Post
        } else {
            erp_modules::migrations::MigrationStage::End
        };
        assert!(
            matches!(&error,erp_modules::error::Error::StepFailed{module,step,file:Some(_),..} if *module==n("property_upgrade") && *step==erp_modules::error::ModuleStep::Migration{stage:expected_stage}),
            "{error:#?}"
        );
        if let erp_modules::error::Error::StepFailed {
            file: Some(file), ..
        } = &error
        {
            // The temporary addon-root prefix is selected by the host fixture.
            assert!(file.ends_with(&format!("migrations/1.1/{stage}-failure.sql")));
            assert_eq!(
                error.to_string(),
                format!("Module \"property_upgrade\": {stage} migration '{file}' failed")
            );
        }
        let conn = db.pool().get().await.unwrap();
        let row=conn.query_one("SELECT note,pg_typeof(note)::text,col_description('upgrade_property'::regclass,(SELECT attnum FROM pg_attribute WHERE attrelid='upgrade_property'::regclass AND attname='note')) FROM upgrade_property",&[]).await.unwrap();
        assert_eq!(row.get::<_, String>(0), "legacy");
        assert_eq!(row.get::<_, String>(1), "character varying");
        assert_eq!(row.get::<_, String>(2), "old comment");
        let row = conn
            .query_one(
                "SELECT state,latest_version FROM ir_module_module WHERE name='property_upgrade'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "installed");
        assert_eq!(row.get::<_, String>(1), "1.0.0");
        assert!(
            conn.query_one("SELECT to_regclass('preserved_id') IS NOT NULL", &[])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        drop(conn);
        addons.file(
            "property_upgrade",
            &format!("migrations/1.1/{stage}-failure.sql"),
            "SELECT 1;",
        );
        let target = configured(&db, &addons, true, previous).build().unwrap();
        let outcome = target
            .button_immediate_upgrade(
                &target.system_env(Default::default()),
                &[n("property_upgrade")],
            )
            .await
            .unwrap();
        assert_eq!(
            rows(&outcome.orm, "SELECT note->>'1' FROM upgrade_property").await,
            vec![vec![Some("legacy".to_owned())]]
        );
        let repeat = target
            .button_immediate_upgrade(&su(&outcome.orm), &[n("property_upgrade")])
            .await
            .unwrap();
        assert_eq!(
            rows(&repeat.orm, "SELECT note->>'1' FROM upgrade_property").await,
            vec![vec![Some("legacy".to_owned())]]
        );
        db.cleanup().await.unwrap();
    }
}
