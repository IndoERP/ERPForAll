//! Genuine base 1.2 binary/schema upgrade rehearsal. Invoked explicitly on an approved build host.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/runtime_addons.rs"]
mod runtime_addons;
#[path = "support/company_upgrade.rs"]
mod support;
use erp_testkit::TestDb;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// the membership trigger on `res_company` (catalog shape and its bump on `active` only) and the
/// restrict rule of the parent foreign key)
#[tokio::test]
#[ignore = "requires ERP_COMPANY_BRANCH_OLD_SOURCE and ERP_COMPANY_BRANCH_OLD_BIN on approved host"]
async fn genuine_previous_company_schema_upgrade_preserves_identity_and_rolls_back() {
    let old = PathBuf::from(
        std::env::var_os("ERP_COMPANY_BRANCH_OLD_SOURCE").expect("old source required"),
    );
    let old_bin =
        PathBuf::from(std::env::var_os("ERP_COMPANY_BRANCH_OLD_BIN").expect("old binary required"));
    let old_revision = fs::read_to_string(old.join("REVISION")).unwrap();
    let old_revision = old_revision.trim();
    assert_eq!(
        old_revision,
        std::env::var("ERP_COMPANY_BRANCH_OLD_REVISION").unwrap()
    );
    assert!(
        fs::read_to_string(old.join("addons/base/manifest.toml"))
            .unwrap()
            .contains("version = \"1.2\"")
    );
    eprintln!(
        "previous_source={old_revision} previous_binary_sha256={}",
        erp_modules::package::sha256_hex(&fs::read(&old_bin).unwrap())
    );
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    let role = format!("{}_branches", db.name());
    client.batch_execute(&format!("CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",erp_sql::quote(&role))).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path().canonicalize().unwrap();
    let old_addons = directory.join("old-addons");
    runtime_addons::stage_runtime_addons(&old.join("addons"), &old_addons).unwrap();
    let fixture = old_addons.join("test_company_branch_upgrade");
    fs::create_dir(&fixture).unwrap();
    fs::write(
        fixture.join("manifest.toml"),
        "name='Company upgrade fixture'\nversion='1.0'\ndepends=['base']\ndata=['companies.xml']\n",
    )
    .unwrap();
    fs::write(fixture.join("companies.xml"),r#"<erp noupdate="1">
      <record id="tenant_root" model="res.company"><field name="name">Tenant Root</field><field name="currency_id" ref="base.EUR"/><field name="street">Tenant street</field></record>
      <record id="tenant_branch" model="res.company"><field name="name">Tenant Branch</field><field name="parent_id" ref="tenant_root"/><field name="currency_id" ref="base.EUR"/></record>
    </erp>"#).unwrap();
    let old_config = support::config(&directory, &db, &role, &old_addons);
    support::private(
        &directory.join("password"),
        "fixture-administrator-password",
    );
    support::succeeded(
        support::run(
            &old_bin,
            &[
                "db",
                "init",
                "--config",
                old_config.to_str().unwrap(),
                "--admin-login",
                "company-upgrade-admin",
                "--admin-password-file",
                directory.join("password").to_str().unwrap(),
            ],
            &db,
        ),
        "previous-db-init",
    );
    support::succeeded(
        support::run(
            &old_bin,
            &[
                "module",
                "install",
                "test_company_branch_upgrade",
                "-d",
                db.name(),
                "--addons-path",
                old_addons.to_str().unwrap(),
            ],
            &db,
        ),
        "previous-fixture-install",
    );
    let previous_version: String = client
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name='base'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(previous_version, "1.2.0");
    let previous_base_digest: String = client
        .query_one(
            "SELECT content_hash FROM installed_addon_identity WHERE name='base'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let absent = async || {
        client.query_one("SELECT NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema='public' AND table_name='res_company' AND column_name='active')",&[]).await.unwrap().get::<_,bool>(0)
    };
    assert!(
        absent().await,
        "previous binary must genuinely predate company active state"
    );
    let root: i64 = client.query_one("SELECT res_id::int8 FROM ir_model_data WHERE module='test_company_branch_upgrade' AND name='tenant_root'",&[]).await.unwrap().get(0);
    let branch: i64 = client.query_one("SELECT res_id::int8 FROM ir_model_data WHERE module='test_company_branch_upgrade' AND name='tenant_branch'",&[]).await.unwrap().get(0);
    let before = client
        .query_one(
            "SELECT name,partner_id,currency_id,parent_path FROM res_company WHERE id=$1",
            &[&root],
        )
        .await
        .unwrap();
    let identity = (
        before.get::<_, String>(0),
        before.get::<_, i64>(1),
        before.get::<_, i64>(2),
        before.get::<_, String>(3),
    );
    let before_contact: Option<String> = client
        .query_one("SELECT street FROM res_partner WHERE id=$1", &[&identity.1])
        .await
        .unwrap()
        .get(0);
    let previous = directory.join("previous");
    fs::create_dir(&previous).unwrap();
    runtime_addons::stage_runtime_addons(&old_addons, &previous.join("addons")).unwrap();
    support::release_inventory(&previous, old_revision, Some(&client)).await;
    let candidate = directory.join("candidate");
    fs::create_dir(&candidate).unwrap();
    let current = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let candidate_version = erp_modules::manifest::Manifest::from_toml(
        "base",
        &fs::read_to_string(current.join("addons/base/manifest.toml")).unwrap(),
    )
    .unwrap()
    .version()
    .to_string();
    runtime_addons::stage_runtime_addons(&current.join("addons"), &candidate.join("addons"))
        .unwrap();
    fs::create_dir(candidate.join("addons/test_company_branch_upgrade")).unwrap();
    for name in ["manifest.toml", "companies.xml"] {
        fs::copy(
            fixture.join(name),
            candidate
                .join("addons/test_company_branch_upgrade")
                .join(name),
        )
        .unwrap();
    }
    let source = fs::read_to_string(current.join("REVISION")).unwrap();
    support::release_inventory(&candidate, source.trim(), None).await;
    let binary = Path::new(env!("CARGO_BIN_EXE_erp"));
    let upgrade = || {
        support::run(
            binary,
            &[
                "module",
                "upgrade",
                "base",
                "-d",
                db.name(),
                "--addons-path",
                candidate.join("addons").to_str().unwrap(),
                "--identity-previous",
                previous.to_str().unwrap(),
            ],
            &db,
        )
    };
    let wrong: i64 = client
        .query_one(
            "SELECT id FROM res_currency WHERE id<>$1 ORDER BY id LIMIT 1",
            &[&identity.2],
        )
        .await
        .unwrap()
        .get(0);
    client
        .execute(
            "UPDATE res_company SET currency_id=$1 WHERE id=$2",
            &[&wrong, &branch],
        )
        .await
        .unwrap();
    let refused = upgrade();
    assert_eq!(refused.status.code(), Some(4));
    // The outer diagnostic includes the native migration's source line, which changes with code layout.
    assert!(String::from_utf8_lossy(&refused.stderr).contains(&format!("base 1.5 company hierarchy/currency validation failed for company {branch}; repair parent paths and branch currencies before upgrading.")));
    eprintln!(
        "step=legacy-currency-refusal status={} diagnostic={}",
        refused.status,
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(absent().await, "failed migration must roll back new column");
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='base'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        previous_version
    );
    assert_eq!(
        client
            .query_one(
                "SELECT content_hash FROM installed_addon_identity WHERE name='base'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        previous_base_digest
    );
    client
        .execute(
            "UPDATE res_company SET currency_id=$1 WHERE id=$2",
            &[&identity.2, &branch],
        )
        .await
        .unwrap();
    client.batch_execute("CREATE FUNCTION fixture_company_late_refusal() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.name='base' AND NEW.latest_version IS DISTINCT FROM OLD.latest_version THEN IF NOT EXISTS (SELECT 1 FROM res_company WHERE active IS TRUE) THEN RAISE EXCEPTION 'fixture active backfill missing'; END IF; RAISE EXCEPTION 'fixture late company publication refusal' USING ERRCODE='23514'; END IF; RETURN NEW; END $$; CREATE TRIGGER fixture_company_late_refusal BEFORE UPDATE OF latest_version ON ir_module_module FOR EACH ROW EXECUTE FUNCTION fixture_company_late_refusal()").await.unwrap();
    let refused = upgrade();
    eprintln!(
        "step=late-publication-refusal status={} diagnostic={}",
        refused.status,
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        std::str::from_utf8(&refused.stderr).unwrap().trim(),
        "Error: The database refused the change: fixture late company publication refusal: db error: ERROR: fixture late company publication refusal"
    );
    assert!(
        absent().await,
        "late publication failure must roll back new column"
    );
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='base'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        previous_version
    );
    assert_eq!(
        client
            .query_one(
                "SELECT content_hash FROM installed_addon_identity WHERE name='base'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        previous_base_digest
    );
    client.batch_execute("DROP TRIGGER fixture_company_late_refusal ON ir_module_module; DROP FUNCTION fixture_company_late_refusal()").await.unwrap();
    support::succeeded(upgrade(), "candidate-upgrade");
    support::succeeded(upgrade(), "candidate-repeat");
    // The upgraded schema carries the trigger that bumps the membership generation when a
    // company is archived or restored (a user's allowed companies depend on it), and the parent
    // foreign key refuses to delete a company that still has branches.
    let trigger = client
        .query_one(
            "SELECT t.tgenabled::text, pg_get_triggerdef(t.oid), obj_description(t.oid, 'pg_trigger') \
               FROM pg_trigger t \
              WHERE t.tgrelid = 'public.res_company'::regclass \
                AND t.tgname = 'res_company_membership_bump' AND NOT t.tgisinternal",
            &[],
        )
        .await
        .expect("the upgrade must leave the res_company membership trigger");
    assert_eq!(trigger.get::<_, String>(0), "O", "trigger must be enabled");
    let definition: String = trigger.get(1);
    assert!(
        definition.contains("UPDATE OF active"),
        "trigger must fire on UPDATE OF active: {definition}"
    );
    assert!(
        definition.contains("FOR EACH STATEMENT"),
        "trigger must be a statement trigger: {definition}"
    );
    assert_eq!(
        trigger.get::<_, Option<String>>(2).as_deref(),
        Some(
            "erpforall signal trigger v1: bump membership on INSERT OR DELETE OR TRUNCATE OR UPDATE OF \"active\""
        )
    );
    // Behavior, inside a transaction that is rolled back: the membership generation moves when
    // `active` is written and does not move when another column is.
    let membership_rows = async || {
        client
            .query_one(
                "SELECT COALESCE(sum(weight), 0)::int8 FROM public.erp_signal_generation WHERE key = 'membership'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
    };
    let generation_before = membership_rows().await;
    client.batch_execute("BEGIN").await.unwrap();
    client
        .execute("UPDATE res_company SET name = name WHERE id = $1", &[&root])
        .await
        .unwrap();
    assert_eq!(
        membership_rows().await,
        generation_before,
        "writing another column must not bump the membership generation"
    );
    client
        .execute(
            "UPDATE res_company SET active = NOT active WHERE id = $1",
            &[&root],
        )
        .await
        .unwrap();
    assert_eq!(
        membership_rows().await,
        generation_before + 1,
        "writing active must bump the membership generation once"
    );
    client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(
        membership_rows().await,
        generation_before,
        "the rolled back probe must leave the generation as it was"
    );
    assert_eq!(
        client
            .query_one("SELECT active FROM res_company WHERE id = $1", &[&root])
            .await
            .unwrap()
            .get::<_, Option<bool>>(0),
        Some(true),
        "the probe must not change the stored state"
    );
    let delete_rule: String = client
        .query_one(
            "SELECT c.confdeltype::text FROM pg_constraint c \
               JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey) \
              WHERE c.conrelid = 'public.res_company'::regclass \
                AND c.confrelid = 'public.res_company'::regclass \
                AND c.contype = 'f' AND a.attname = 'parent_id'",
            &[],
        )
        .await
        .expect("res_company.parent_id must have exactly one foreign key to res_company")
        .get(0);
    assert_eq!(delete_rule, "r", "parent_id must restrict deletion");
    let after = client
        .query_one(
            "SELECT name,partner_id,currency_id,parent_path,active FROM res_company WHERE id=$1",
            &[&root],
        )
        .await
        .unwrap();
    assert_eq!(
        (
            after.get::<_, String>(0),
            after.get::<_, i64>(1),
            after.get::<_, i64>(2),
            after.get::<_, String>(3)
        ),
        identity
    );
    assert!(after.get::<_, bool>(4));
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM res_company WHERE active IS NOT TRUE",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client
            .query_one("SELECT street FROM res_partner WHERE id=$1", &[&identity.1])
            .await
            .unwrap()
            .get::<_, Option<String>>(0),
        before_contact
    );
    let branch_after = client
        .query_one(
            "SELECT parent_id,currency_id FROM res_company WHERE id=$1",
            &[&branch],
        )
        .await
        .unwrap();
    assert_eq!(branch_after.get::<_, i64>(0), root);
    assert_eq!(branch_after.get::<_, i64>(1), identity.2);
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='base'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        candidate_version
    );
    let new_config_dir = directory.join("new-config");
    fs::create_dir(&new_config_dir).unwrap();
    let new_config = support::config(&new_config_dir, &db, &role, &candidate.join("addons"));
    support::succeeded(
        support::run(
            binary,
            &[
                "db",
                "publish-grants",
                "--config",
                new_config.to_str().unwrap(),
            ],
            &db,
        ),
        "candidate-runtime-grants",
    );
    assert!(
        client
            .query_one(
                "SELECT has_table_privilege($1, 'res_company_lifecycle_fence', 'SELECT') AND has_table_privilege($1, 'res_company_lifecycle_fence', 'UPDATE')",
                &[&role]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(client);
    db.cleanup().await.unwrap();
    // Test-created roles are cluster-global and are removed explicitly after their database.
    let cluster = TestDb::create().await.unwrap();
    cluster
        .client()
        .await
        .unwrap()
        .batch_execute(&format!("DROP ROLE {}", erp_sql::quote(&role)))
        .await
        .unwrap();
    cluster.cleanup().await.unwrap();
}
